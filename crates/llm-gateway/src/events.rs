//! 転送のたびに起きたことを、見ている人へ流す (DR-0012)。
//!
//! gateway は全部のリクエストを仲介しているので、**upstream が応答を返した
//! 瞬間**を知っている唯一の場所になる。prompt cache の 5 分は upstream が
//! 前処理を始めた時点から走るので、外から見える最良の近似がこの瞬間になる。
//!
//! **時刻の欄は数値 1 つ = Unix ミリ秒**で出す (DR-0012)。長さの欄だけが
//! 名前に単位を持つ (`cache_ttl_secs`)。人が読む形へ直すのは受け取った側の
//! 仕事で、同じ時刻を 2 通りの形で並べない。
//!
//! 流すのは起きたことだけで、状態は持たない。誰も見ていなければ何もしない。
//! 見ている人が遅れたら、その人の分は落ちる — 5 分の残りを数える相手に、
//! 遅れて届いた開始時刻を渡しても使い道がない。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::cache::keepalive::{Breakeven, Chain};
use crate::denial::Reason;

/// 系列の識別子として出すハッシュの長さ (16 進の桁数)。
///
/// 見分けが付けば足りる。人がログで突き合わせるので、短いほうが読める。
const PREFIX_DIGITS: usize = 8;

/// 溜めておける数。
///
/// 見ている人が遅れた分はここを溢れて落ちる。大きくしても「古い開始時刻が
/// まとめて届く」だけで、数え直す相手の役には立たない。
const BACKLOG: usize = 256;

/// 経路選定で外した経路と、その理由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub credential: String,
    pub reason: Reason,
}

/// upstream が応答を返した、という知らせ。
///
/// 時刻の欄はすべて Unix ミリ秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// この知らせの時刻 = upstream へこの 1 本を送り始めた瞬間。
    ///
    /// 合図の連鎖 (`cache_*`) から見れば、ここが「今」になる。
    pub ts: i64,
    /// どの会話か。ヘッダを付けてこないクライアントでは `null`。
    pub session_id: Option<String>,
    /// どの namespace 宛か。
    pub ns: String,
    /// 解決後の実モデル名 (`opus` のような短い名前はここでは解決済み)。
    pub model: String,
    /// 答えた経路の名前 (= 設定に書いた credential の名前)。
    pub credential: String,
    pub status: u16,
    /// この会話系列の識別子 ([`prefix`])。取れなければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// この 1 本を出した側 (`main` / `sub` / `unknown`、DR-0024)。
    pub origin: String,
    /// この 1 本が残すプレフィックスの寿命 (**秒**)。時刻ではなく長さなので、
    /// 名前に単位を持つ。cache を使わない 1 本では欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_secs: Option<u64>,
    /// その寿命が尽きる時刻。`ts` + [`Self::cache_ttl_secs`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_expires_at: Option<i64>,
    /// 経路選定で外した経路。無ければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// この 1 本が cache の合図 (DR-0024 §2) だったときの扱い。間に合った分は
    /// `applied`、遅れて 1 時間を付けなかった分は `late`。合図でなければ出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive: Option<String>,
    /// この会話への合図が止めてあるか (DR-0024 §2 追補)。**常に出す** — 見る側は
    /// 毎回の知らせで塗り替えるので、欄が消えると「止まっていない」と区別が
    /// 付かない。
    pub cache_paused: bool,
    /// 合図の連鎖の起点 = この系列で最後に来た実リクエストを送った時刻。
    /// 合図の往復の知らせでも、起点の実リクエストの時刻を出す。
    ///
    /// ここから下の `cache_*` は、合図の見張りが付いている系列 (`keepalive`
    /// 戦略が効く本流) にだけ出る。付いていなければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_since: Option<i64>,
    /// 次の合図の予定時刻。次が無ければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_keepalive_at: Option<i64>,
    /// この 1 本が連鎖の何番目か。実リクエストは 0、k 回目の合図は k。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_count: Option<u32>,
    /// この系列の cache を、合図で継ぎ足せる終わり。
    ///
    /// [`Self::cache_expires_at`] が「この 1 本が置いた cache がいつ消えるか」
    /// なのに対して、こちらは**最後に出る合図が置く cache がいつ消えるか**。
    /// 会話が止まったままなら、実際に切れるのはこの時刻になる。
    ///
    /// 合図が出せなかった場合 (経路が塞がる・戻りが `late`) は、ここより
    /// 早く切れる。見る側は最新の知らせで上書きする。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_until: Option<i64>,
    /// 連鎖で出す合図の総数。[`Self::cache_until`] を作る合図の番号でもある。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_until_count: Option<u32>,
    /// 損益分岐時間まで繋いだ場合の終わり (DR-0024 §3)。
    ///
    /// 単価が分からず分岐時間を出せないモデルでは欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_breakeven_until: Option<i64>,
    /// 分岐時間に収まる合図の本数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_breakeven_count: Option<u32>,
}

/// 知らせに載せる、この呼び出しの素性。
///
/// 位置で渡すと、同じ型の欄 (会話の id と系列の id) を取り違えても気づけない。
/// 名前で書けるようにしておく。
///
/// 借りた文字列だけなので写して構わない。1 本の転送で経路ごとに何度も知らせを
/// 組むので、素性は組み立て直さずに欄だけ差し替える。
#[derive(Clone, Copy)]
pub struct Origin<'a> {
    /// クライアントが名乗った会話の id。
    pub session_id: Option<&'a str>,
    /// 会話系列の識別子。
    pub prefix: Option<&'a str>,
    pub ns: &'a str,
    /// 解決後の実モデル名。
    pub model: &'a str,
    /// 答えた経路の名前。
    pub credential: &'a str,
    /// この 1 本を出した側 (`main` / `sub` / `unknown`)。
    pub origin: &'a str,
    /// この 1 本が残すプレフィックスの寿命 (秒)。
    pub cache_ttl_secs: Option<u64>,
    /// cache の合図としての扱い ([`Event::keepalive`])。
    pub keepalive: Option<&'a str>,
    /// この会話への合図が止めてあるか ([`Event::cache_paused`])。
    pub cache_paused: bool,
    /// この系列に立っている合図の連鎖 ([`Event::cache_since`] 以下)。
    pub chain: Option<Chain>,
    /// 損益分岐時間から起こした連鎖 ([`Event::cache_breakeven_until`])。
    pub breakeven: Option<Breakeven>,
}

impl Event {
    pub fn new(ts_ms: i64, origin: &Origin<'_>, status: u16) -> Self {
        Self::with_skipped(ts_ms, origin, status, Vec::new())
    }

    pub fn with_skipped(
        ts_ms: i64,
        origin: &Origin<'_>,
        status: u16,
        skipped: Vec<Skipped>,
    ) -> Self {
        Self {
            ts: ts_ms,
            session_id: origin.session_id.map(str::to_owned),
            ns: origin.ns.to_owned(),
            model: origin.model.to_owned(),
            credential: origin.credential.to_owned(),
            status,
            prefix: origin.prefix.map(str::to_owned),
            origin: origin.origin.to_owned(),
            cache_ttl_secs: origin.cache_ttl_secs,
            // 寿命の起点は、この 1 本を upstream へ送り始めた時刻。数える側で
            // 足し算をさせない。
            cache_expires_at: origin
                .cache_ttl_secs
                .map(|ttl_secs| ts_ms + (ttl_secs * 1_000) as i64),
            skipped,
            keepalive: origin.keepalive.map(str::to_owned),
            cache_paused: origin.cache_paused,
            cache_since: origin.chain.map(|chain| chain.since_ms),
            next_keepalive_at: origin.chain.and_then(|chain| chain.next_at_ms),
            cache_count: origin.chain.map(|chain| chain.count),
            cache_until: origin.chain.map(|chain| chain.until_ms),
            cache_until_count: origin.chain.map(|chain| chain.until_count),
            cache_breakeven_until: origin.breakeven.map(|breakeven| breakeven.until_ms),
            cache_breakeven_count: origin.breakeven.map(|breakeven| breakeven.count),
        }
    }
}

/// この会話への合図を止めた、という知らせ (DR-0024 §2 追補)。
///
/// 止めるのは人の意思で、実リクエストとは別の出来事。**止まった瞬間**を
/// 見る側 (ccmsg の webui) へ伝える口がここしかない — 解除は次の実リクエストの
/// [`Event::cache_paused`] で伝わるが、停止には次の 1 本が来ない。
///
/// 兄弟から回ってきた停止では流さない。人から直に受けた instance が既に
/// 流していて、同じ受け口が 2 度受け取ることになる。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepalivePaused {
    /// 受け取る側が種類を見分ける印。値は常に `keepalive_paused`。
    #[serde(rename = "type")]
    pub kind: String,
    /// どの会話が止まったか。
    pub session_id: String,
    /// 止めた時刻 (Unix ミリ秒)。
    pub paused_at: i64,
}

impl KeepalivePaused {
    /// 種類の印。
    pub const KIND: &'static str = "keepalive_paused";

    pub fn new(session_id: &str, paused_at_ms: i64) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            session_id: session_id.to_owned(),
            paused_at: paused_at_ms,
        }
    }
}

/// 会話が止まった、という合図 (DR-0024 §2)。
///
/// 受け取った側 (ccmsg) が [`Self::marker`] をその会話へ流し込むと、戻って
/// きたリクエストに 1 時間の cache が付く。[`Self::deadline`] を過ぎてから
/// 届いたものには付けない — 間に合わなかった合図に 2 倍の書き込みをさせると、
/// 何もしないより高くつく。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keepalive {
    /// 受け取る側が種類を見分ける印。値は常に `cache_keepalive`。
    #[serde(rename = "type")]
    pub kind: String,
    /// この合図を出した時刻 (Unix ミリ秒)。
    pub ts: i64,
    /// どの会話へ流し込むか。
    pub session_id: String,
    /// その会話のどの系列か ([`prefix`])。
    pub prefix: String,
    /// この 1 回きりの合言葉。戻ってきたリクエストの照合に使う。
    pub nonce: String,
    /// これを過ぎて届いたら 1 時間は付かない (Unix ミリ秒)。
    pub deadline: i64,
    /// そのまま会話へ流し込む文面。
    pub marker: String,
}

impl Keepalive {
    /// 種類の印。受け取る側はこの値で [`Event`] と見分ける。
    pub const KIND: &'static str = "cache_keepalive";

    pub fn new(ts_ms: i64, session_id: &str, prefix: &str, nonce: &str, deadline_ms: i64) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            ts: ts_ms,
            session_id: session_id.to_owned(),
            prefix: prefix.to_owned(),
            nonce: nonce.to_owned(),
            deadline: deadline_ms,
            marker: marker(nonce),
        }
    }
}

/// 合言葉の頭。この後ろに nonce が続いたものが 1 つの合言葉になる。
///
/// 会話へ送る文面にも、返させる語にも、戻ってきたリクエストから探すときにも
/// 同じものを使う (= 合言葉は 1 つしか出てこない)。
pub const KEEPALIVE_TOKEN_PREFIX: &str = "LLMGW-KEEPALIVE-";

/// 会話へ流し込む文面。
///
/// 合言葉が 1 度だけ出てくる決め打ちの形にするのは、戻ってきたリクエストの
/// 中から見つけるため。途中に挟まっていても拾えるようにしてあり (合図は通知に
/// 包まれて届く)、返させるのは**その合言葉 1 語だけ**。
///
/// 出所と目的を文面自身に書く。合図は会話の文脈を持たない相手にも届くので、
/// 素性の分からない指示に見えると、注入を疑われて断られる (実測)。
///
/// 「何も出力するな」とは頼まない。それは自分の振る舞いについての指示なので
/// 完全には従わせられず、断り書きが 1 行返ってくる (実測)。返る形が決まって
/// いれば、受け取った側がその 1 行を畳んで見せずに済む。空白を含まない形に
/// するのは、受け取った側が 1 つの語として拾えるようにするため。
pub fn marker(nonce: &str) -> String {
    format!(
        "[llm-gateway keepalive ping] nonce=`{KEEPALIVE_TOKEN_PREFIX}{nonce}` — \
         automated prompt-cache refresh from your own llm-gateway proxy \
         (see llm-gateway docs, DR-0024). Reply with a single line containing \
         only the nonce above, nothing before or after."
    )
}

/// 受け口へ流す 1 件。
///
/// 転送の知らせ ([`Event`]) と cache の合図 ([`Keepalive`]) は別の出来事で、
/// 欄も重ならない。1 つの型に混ぜて空欄で埋めるのではなく、種類として分ける。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Notice {
    /// 転送の知らせ。欄が多く、他の 2 種より大きいので箱に入れる
    /// (この列は 1 件流すたびに写される)。
    Request(Box<Event>),
    CacheKeepalive(Keepalive),
    KeepalivePaused(KeepalivePaused),
}

impl Notice {
    /// SSE の 1 通に付ける名前。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Request(_) => "request",
            Self::CacheKeepalive(_) => Keepalive::KIND,
            Self::KeepalivePaused(_) => KeepalivePaused::KIND,
        }
    }

    /// 転送の知らせなら中身。それ以外なら `None`。
    pub fn request(&self) -> Option<&Event> {
        match self {
            Self::Request(event) => Some(event),
            Self::CacheKeepalive(_) | Self::KeepalivePaused(_) => None,
        }
    }
}

impl From<Event> for Notice {
    fn from(event: Event) -> Self {
        Self::Request(Box::new(event))
    }
}

impl From<Keepalive> for Notice {
    fn from(keepalive: Keepalive) -> Self {
        Self::CacheKeepalive(keepalive)
    }
}

impl From<KeepalivePaused> for Notice {
    fn from(paused: KeepalivePaused) -> Self {
        Self::KeepalivePaused(paused)
    }
}

/// この会話系列の識別子。`system` の**先頭ブロック**のハッシュ。
///
/// 同じ会話の id でも、メインとサブエージェントでは別の system prompt が
/// 使われる。会話の id だけで束ねると、両者の往復が同じ系列に見えてしまう。
///
/// 見るのは**先頭ブロックだけ**。実測 (451 リクエスト) では、`system` 配列の
/// 末尾ブロックに `git status` 由来の内容 (コミット一覧・未コミットの
/// ファイル) が入っていて、リポジトリを触るたびに変わる。全体を見ると、
/// 同じ系列が別物に分かれる。クライアントが必ず先頭に置く固定の 1 行
/// (自分の版を名乗る行) で始まる先頭ブロックは、全リクエストで同一だった。
///
/// **キャッシュに当たる保証ではない** (DR-0012)。upstream の cache は
/// プレフィックス全体の一致を要るので、末尾が変われば実際には効かない。
/// これはあくまで「同じ系列か」を見分けるための印。
///
/// 本文は受け取り口で既に解釈済みなので、ここでは 1 箇所を引くだけで済む
/// (本文の大きさに関わらず、ハッシュを取るのは先頭ブロックだけ)。取れない
/// 形 (`system` が無い / 文字列 / 空 / `text` が無い) では `None`。
pub fn prefix(body: &Value) -> Option<String> {
    let first = body.pointer("/system/0/text")?.as_str()?;
    Some(short_hash(first))
}

/// 先頭 [`PREFIX_DIGITS`] 桁だけの SHA-256。
fn short_hash(text: &str) -> String {
    use sha2::{Digest as _, Sha256};

    Sha256::digest(text.as_bytes())
        .iter()
        .take(PREFIX_DIGITS.div_ceil(2))
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// 見ている人へ配る口。
pub struct Events {
    tx: broadcast::Sender<Notice>,
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl Events {
    pub fn new() -> Self {
        Self {
            tx: broadcast::Sender::new(BACKLOG),
        }
    }

    /// 1 件流す。
    ///
    /// 誰も見ていなければ何もしない。**転送の邪魔をしないこと**が第一で、
    /// 配れなかったことを転送側へ持ち帰らない (待たない・失敗にしない)。
    pub fn publish(&self, notice: impl Into<Notice>) {
        let _ = self.tx.send(notice.into());
    }

    /// 見る側に回る。届くのは**これ以降**の分だけ。
    ///
    /// 過去に遡らないのは、この知らせが「今から 5 分」を数えるためのもの
    /// だから。接続した時点で既に過ぎている分を配っても数え直せない。
    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.tx.subscribe()
    }

    /// 今この口を見ている人の数。
    pub fn watchers(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 試験に使う「今」 (Unix ミリ秒)。
    const NOW: i64 = 1_800_000_000_000;

    /// 1 分・1 時間をミリ秒で。
    const MINUTE: i64 = 60 * 1_000;
    const HOUR: i64 = 60 * MINUTE;

    /// 素性を 1 つ組む。試験で変えたい欄だけを書けるようにする。
    fn from(credential: &str) -> Origin<'_> {
        Origin {
            session_id: None,
            prefix: None,
            ns: "personal",
            model: "m",
            credential,
            keepalive: None,
            cache_paused: false,
            chain: None,
            breakeven: None,
            origin: "main",
            cache_ttl_secs: None,
        }
    }

    /// 転送の知らせとして届いた 1 件。
    fn request(notice: Notice) -> Event {
        match notice {
            Notice::Request(event) => *event,
            other => panic!("expected a forwarding notice, got {other:?}"),
        }
    }

    /// 誰も見ていなくても、流す側は何も気にしない。
    #[test]
    fn publishing_to_nobody_is_fine() {
        let events = Events::new();
        assert_eq!(events.watchers(), 0);
        events.publish(Event::new(NOW, &from("a"), 200));
    }

    #[tokio::test]
    async fn a_watcher_gets_what_happens_next() {
        let events = Events::new();
        let mut watching = events.subscribe();

        events.publish(Event::new(
            NOW,
            &Origin {
                session_id: Some("s-1"),
                model: "claude-fable-5",
                ..from("claude-kawazzz")
            },
            200,
        ));

        let got = request(watching.recv().await.unwrap());
        assert_eq!(got.session_id.as_deref(), Some("s-1"));
        assert_eq!(got.model, "claude-fable-5");
        assert_eq!(got.credential, "claude-kawazzz");
        assert_eq!(got.status, 200);
        assert_eq!(got.ts, NOW);
    }

    /// 見始める前に起きたことは届かない。今から 5 分を数えるための知らせなので、
    /// 過ぎた分を配っても使えない。
    #[tokio::test]
    async fn nothing_is_replayed() {
        let events = Events::new();
        events.publish(Event::new(NOW, &from("a"), 200));

        let mut watching = events.subscribe();
        events.publish(Event::new(NOW + 1, &from("b"), 200));

        let got = request(watching.recv().await.unwrap());
        assert_eq!(
            got.ts,
            NOW + 1,
            "only events after the subscription started"
        );
    }

    /// 断られた応答も流す。上限に当たったことも、見ている側には知らせ。
    #[tokio::test]
    async fn a_denial_is_an_event_too() {
        let events = Events::new();
        let mut watching = events.subscribe();
        events.publish(Event::new(NOW, &from("a"), 429));
        assert_eq!(request(watching.recv().await.unwrap()).status, 429);
    }

    /// system の先頭ブロックが同じなら、同じ系列とみなす。
    ///
    /// 末尾ブロックは `git status` 由来の内容で毎回変わる (実測)。そこまで
    /// 見ると、同じ系列が触るたびに別物へ分かれる。
    #[test]
    fn the_tail_of_the_system_prompt_does_not_change_the_series() {
        let main = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.0.1"},
            {"type": "text", "text": "gitStatus: branch main, clean"},
        ]});
        let later = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.0.1"},
            {"type": "text", "text": "gitStatus: branch main, 3 files changed"},
        ]});

        assert_eq!(prefix(&main), prefix(&later));
        assert!(prefix(&main).is_some());
    }

    /// 先頭ブロックが違えば別の系列。サブエージェントはここが違う。
    #[test]
    fn a_different_first_block_is_a_different_series() {
        let main = json!({"system": [{"type": "text", "text": "You are Claude Code"}]});
        let sub = json!({"system": [{"type": "text", "text": "You are a subagent"}]});
        assert_ne!(prefix(&main), prefix(&sub));
    }

    /// 見分けが付く長さの 16 進で出す。
    #[test]
    fn the_series_id_is_short_and_hexadecimal() {
        let id = prefix(&json!({"system": [{"type": "text", "text": "hello"}]})).unwrap();
        assert_eq!(id.len(), PREFIX_DIGITS);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        // SHA-256("hello") の先頭。値を固定しておくと、取り方を変えたときに気づく。
        assert_eq!(id, "2cf24dba");
    }

    /// 系列が分からない形では、欄ごと出さない。どれも落ちない。
    #[test]
    fn an_unreadable_system_prompt_has_no_series() {
        for body in [
            json!({}),
            json!({"system": "むかしの書き方 (文字列)"}),
            json!({"system": []}),
            json!({"system": [{"type": "text"}]}),
            json!({"system": [{"type": "text", "text": 42}]}),
            json!({"system": null}),
            json!(null),
            json!("まるごと文字列"),
        ] {
            assert_eq!(prefix(&body), None, "{body}");
        }
    }

    /// 本文が大きくても、ハッシュを取るのは先頭ブロックだけ。
    ///
    /// 時間を測ると相手のマシンの都合で揺れるので、**結果が本文の大きさに
    /// 依らない**ことで見る。
    #[test]
    fn only_the_first_block_is_hashed() {
        let head = "x-anthropic-billing-header: cc_version=2.0.1";
        let small = json!({"system": [{"type": "text", "text": head}]});
        let huge = json!({"system": [
            {"type": "text", "text": head},
            {"type": "text", "text": "z".repeat(4 * 1024 * 1024)},
        ]});
        assert_eq!(prefix(&small), prefix(&huge));
    }

    /// JSON の形。見る側 (ccmsg 等) が読む契約なので、欄の名前を固定する。
    #[test]
    fn the_json_shape_is_fixed() {
        let event = Event::new(
            NOW,
            &Origin {
                session_id: Some("s-1"),
                ..from("a")
            },
            200,
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["ts"], NOW);
        assert_eq!(json["session_id"], "s-1");
        assert_eq!(json["ns"], "personal");
        assert_eq!(json["model"], "m");
        assert_eq!(json["credential"], "a");
        assert_eq!(json["status"], 200);
        assert!(
            json.get("prefix").is_none(),
            "omits the field entirely when the series is unknown"
        );
        assert!(
            json.get("skipped").is_none(),
            "omits the field entirely when every route was eligible"
        );

        let skipped = Event::with_skipped(
            NOW,
            &from("a"),
            200,
            vec![
                Skipped {
                    credential: "limited-route".to_owned(),
                    reason: Reason::Limited,
                },
                Skipped {
                    credential: "busy-route".to_owned(),
                    reason: Reason::Busy,
                },
                Skipped {
                    credential: "paced-route".to_owned(),
                    reason: Reason::Paced,
                },
            ],
        );
        assert_eq!(
            serde_json::to_value(&skipped).unwrap()["skipped"],
            json!([
                {"credential": "limited-route", "reason": "limited"},
                {"credential": "busy-route", "reason": "busy"},
                {"credential": "paced-route", "reason": "paced"},
            ]),
            "each internal reason has one stable lowercase output word"
        );

        let cached = Event::new(
            NOW,
            &Origin {
                origin: "sub",
                cache_ttl_secs: Some(3600),
                ..from("a")
            },
            200,
        );
        let json = serde_json::to_value(&cached).unwrap();
        assert_eq!(json["origin"], "sub");
        assert_eq!(
            json["cache_ttl_secs"], 3600,
            "a length keeps its unit in the name"
        );
        assert_eq!(
            json["cache_expires_at"],
            NOW + HOUR,
            "counted from the moment the request went out, in milliseconds"
        );

        let uncached = serde_json::to_value(Event::new(NOW, &from("a"), 200)).unwrap();
        assert_eq!(uncached["origin"], "main", "the field is always there");
        for field in ["cache_ttl_secs", "cache_expires_at"] {
            assert!(
                uncached.get(field).is_none(),
                "{field} is omitted when nothing is cached"
            );
        }

        let in_series = Event::new(
            NOW,
            &Origin {
                prefix: Some("2cf24dba"),
                ..from("a")
            },
            200,
        );
        assert_eq!(
            serde_json::to_value(&in_series).unwrap()["prefix"],
            "2cf24dba"
        );

        // 会話が分からない場合も欄は残す (欠けると、読む側が形を 2 通り扱う)。
        let nameless = Event::new(NOW, &from("a"), 200);
        assert!(serde_json::to_value(&nameless).unwrap()["session_id"].is_null());

        let watched = Event::new(
            NOW,
            &Origin {
                chain: Some(Chain {
                    since_ms: NOW - 10 * MINUTE,
                    count: 2,
                    next_at_ms: Some(NOW + 55 * MINUTE),
                    until_ms: NOW + 9 * HOUR,
                    until_count: 5,
                }),
                breakeven: Some(Breakeven {
                    count: 12,
                    until_ms: NOW + 15 * HOUR,
                }),
                ..from("a")
            },
            200,
        );
        let json = serde_json::to_value(&watched).unwrap();
        assert_eq!(json["cache_since"], NOW - 10 * MINUTE);
        assert_eq!(json["next_keepalive_at"], NOW + 55 * MINUTE);
        assert_eq!(json["cache_count"], 2);
        assert_eq!(json["cache_until"], NOW + 9 * HOUR);
        assert_eq!(json["cache_until_count"], 5);
        assert_eq!(json["cache_breakeven_until"], NOW + 15 * HOUR);
        assert_eq!(json["cache_breakeven_count"], 12);
        assert!(
            json.as_object()
                .unwrap()
                .keys()
                .all(|key| !key.ends_with("_iso")),
            "times are one number each: {json}"
        );

        // 単価の分からないモデルでは、分岐点だけが欠ける。
        let unpriced = serde_json::to_value(Event::new(
            NOW,
            &Origin {
                chain: Some(Chain {
                    since_ms: NOW,
                    count: 0,
                    next_at_ms: None,
                    until_ms: NOW + 9 * HOUR,
                    until_count: 5,
                }),
                ..from("a")
            },
            200,
        ))
        .unwrap();
        assert_eq!(unpriced["cache_until"], NOW + 9 * HOUR);
        for field in [
            "cache_breakeven_until",
            "cache_breakeven_count",
            // 最後の 1 本を出し終えた系列には、次の予定が無い。
            "next_keepalive_at",
        ] {
            assert!(unpriced.get(field).is_none(), "{field} is omitted");
        }

        // 止まりは常に出る (欄が消えると「止まっていない」と区別が付かない)。
        let quiet = serde_json::to_value(Event::new(NOW, &from("a"), 200)).unwrap();
        assert_eq!(quiet["cache_paused"], false);
        let paused = serde_json::to_value(Event::new(
            NOW,
            &Origin {
                cache_paused: true,
                ..from("a")
            },
            200,
        ))
        .unwrap();
        assert_eq!(paused["cache_paused"], true);

        for field in [
            "cache_since",
            "next_keepalive_at",
            "cache_count",
            "cache_until",
            "cache_until_count",
        ] {
            assert!(
                quiet.get(field).is_none(),
                "{field} is omitted when no signal watches this series"
            );
        }
    }

    /// 3 種の知らせの全文。欄の名前・並び・単位が、見る側との契約になる。
    ///
    /// 個々の欄は上の試験で見ているので、ここでは**丸ごと 1 通**を固定する
    /// — 欄が増えたり単位が変わったりしたら、ここが落ちる。
    #[test]
    fn a_whole_notice_is_settled() {
        let forwarded = Event::new(
            NOW,
            &Origin {
                session_id: Some("s-1"),
                prefix: Some("2cf24dba"),
                model: "claude-opus-5",
                origin: "main",
                cache_ttl_secs: Some(3600),
                chain: Some(Chain {
                    since_ms: NOW,
                    count: 0,
                    next_at_ms: Some(NOW + 55 * MINUTE),
                    until_ms: NOW + 9 * 55 * MINUTE + HOUR,
                    until_count: 9,
                }),
                breakeven: Some(Breakeven {
                    count: 20,
                    until_ms: NOW + 20 * 55 * MINUTE + HOUR,
                }),
                ..from("personal")
            },
            200,
        );
        assert_eq!(
            serde_json::to_value(&forwarded).unwrap(),
            json!({
                "ts": NOW,
                "session_id": "s-1",
                "ns": "personal",
                "model": "claude-opus-5",
                "credential": "personal",
                "status": 200,
                "prefix": "2cf24dba",
                "origin": "main",
                "cache_ttl_secs": 3600,
                "cache_expires_at": NOW + HOUR,
                "cache_paused": false,
                "cache_since": NOW,
                "next_keepalive_at": NOW + 55 * MINUTE,
                "cache_count": 0,
                "cache_until": NOW + 9 * 55 * MINUTE + HOUR,
                "cache_until_count": 9,
                "cache_breakeven_until": NOW + 20 * 55 * MINUTE + HOUR,
                "cache_breakeven_count": 20,
            })
        );

        let answered = Event::new(
            NOW + 55 * MINUTE,
            &Origin {
                session_id: Some("s-1"),
                prefix: Some("2cf24dba"),
                model: "claude-opus-5",
                origin: "main",
                cache_ttl_secs: Some(3600),
                keepalive: Some("applied"),
                chain: Some(Chain {
                    since_ms: NOW,
                    count: 1,
                    next_at_ms: Some(NOW + 2 * 55 * MINUTE),
                    until_ms: NOW + 9 * 55 * MINUTE + HOUR,
                    until_count: 9,
                }),
                ..from("personal")
            },
            200,
        );
        assert_eq!(
            serde_json::to_value(&answered).unwrap(),
            json!({
                "ts": NOW + 55 * MINUTE,
                "session_id": "s-1",
                "ns": "personal",
                "model": "claude-opus-5",
                "credential": "personal",
                "status": 200,
                "prefix": "2cf24dba",
                "origin": "main",
                "cache_ttl_secs": 3600,
                "cache_expires_at": NOW + 55 * MINUTE + HOUR,
                "keepalive": "applied",
                "cache_paused": false,
                "cache_since": NOW,
                "next_keepalive_at": NOW + 2 * 55 * MINUTE,
                "cache_count": 1,
                "cache_until": NOW + 9 * 55 * MINUTE + HOUR,
                "cache_until_count": 9,
            }),
            "the round trip of a signal moves the chain along"
        );

        let signal = Keepalive::new(
            NOW + 55 * MINUTE,
            "s-1",
            "2cf24dba",
            "5Qv",
            NOW + HOUR - 30 * 1_000,
        );
        assert_eq!(
            serde_json::to_value(&signal).unwrap(),
            json!({
                "type": "cache_keepalive",
                "ts": NOW + 55 * MINUTE,
                "session_id": "s-1",
                "prefix": "2cf24dba",
                "nonce": "5Qv",
                "deadline": NOW + HOUR - 30 * 1_000,
                "marker": marker("5Qv"),
            })
        );

        assert_eq!(
            serde_json::to_value(Notice::from(KeepalivePaused::new("s-1", NOW))).unwrap(),
            json!({
                "type": "keepalive_paused",
                "session_id": "s-1",
                "paused_at": NOW,
            })
        );
    }

    /// 止めた知らせの形。合図とも転送とも混ざらない。
    #[test]
    fn a_pause_is_told_apart_by_its_type() {
        let notice = Notice::from(KeepalivePaused::new("s-1", NOW));
        assert_eq!(notice.name(), "keepalive_paused");
        assert!(notice.request().is_none());

        let json = serde_json::to_value(&notice).unwrap();
        assert_eq!(json["type"], "keepalive_paused");
        assert_eq!(json["session_id"], "s-1");
        assert_eq!(json["paused_at"], NOW, "a single number, in milliseconds");

        // 受け取る側は 1 つの列で読む。型を跨いで取り違えない。
        assert_eq!(
            serde_json::from_value::<Notice>(json).unwrap(),
            notice,
            "it reads back as the same kind of notice"
        );
    }
}
