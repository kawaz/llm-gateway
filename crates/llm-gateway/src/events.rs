//! 転送のたびに起きたことを、見ている人へ流す (DR-0012)。
//!
//! gateway は全部のリクエストを仲介しているので、**upstream が応答を返した
//! 瞬間**を知っている唯一の場所になる。prompt cache の 5 分は upstream が
//! 前処理を始めた時点から走るので、外から見える最良の近似がこの瞬間になる。
//!
//! 流すのは起きたことだけで、状態は持たない。誰も見ていなければ何もしない。
//! 見ている人が遅れたら、その人の分は落ちる — 5 分の残りを数える相手に、
//! 遅れて届いた開始時刻を渡しても使い道がない。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::credential::time::format_rfc3339;
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// 応答のヘッダを受け取った時刻 (Unix 秒)。
    pub ts: i64,
    /// 同じ時刻の ISO 8601 表記。人が読む側で変換し直さずに済む。
    pub ts_iso: String,
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
    /// 経路選定で外した経路。無ければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// この 1 本が cache の合図 (DR-0024 §2) だったときの扱い。間に合った分は
    /// `applied`、遅れて 1 時間を付けなかった分は `late`。合図でなければ出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive: Option<String>,
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
    /// cache の合図としての扱い ([`Event::keepalive`])。
    pub keepalive: Option<&'a str>,
}

impl Event {
    pub fn new(ts: i64, origin: &Origin<'_>, status: u16) -> Self {
        Self::with_skipped(ts, origin, status, Vec::new())
    }

    pub fn with_skipped(ts: i64, origin: &Origin<'_>, status: u16, skipped: Vec<Skipped>) -> Self {
        Self {
            ts,
            ts_iso: format_rfc3339(ts),
            session_id: origin.session_id.map(str::to_owned),
            ns: origin.ns.to_owned(),
            model: origin.model.to_owned(),
            credential: origin.credential.to_owned(),
            status,
            prefix: origin.prefix.map(str::to_owned),
            skipped,
            keepalive: origin.keepalive.map(str::to_owned),
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
    pub ts: i64,
    pub ts_iso: String,
    /// どの会話へ流し込むか。
    pub session_id: String,
    /// その会話のどの系列か ([`prefix`])。
    pub prefix: String,
    /// この 1 回きりの合言葉。戻ってきたリクエストの照合に使う。
    pub nonce: String,
    /// これを過ぎて届いたら 1 時間は付かない (Unix 秒)。
    pub deadline: i64,
    pub deadline_iso: String,
    /// そのまま会話へ流し込む文面。
    pub marker: String,
}

impl Keepalive {
    /// 種類の印。受け取る側はこの値で [`Event`] と見分ける。
    pub const KIND: &'static str = "cache_keepalive";

    pub fn new(ts: i64, session_id: &str, prefix: &str, nonce: &str, deadline: i64) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            ts,
            ts_iso: format_rfc3339(ts),
            session_id: session_id.to_owned(),
            prefix: prefix.to_owned(),
            nonce: nonce.to_owned(),
            deadline,
            deadline_iso: format_rfc3339(deadline),
            marker: marker(nonce),
        }
    }
}

/// 会話へ流し込む文面。
///
/// 先頭を決め打ちの形にするのは、戻ってきたリクエストの中から見つけるため。
/// 途中に挟まっていても拾えるようにしてあり (合図は通知に包まれて届く)、
/// 返させるのは**空白を含まない 1 つのトークンだけ**。
///
/// 「何も出力するな」とは頼まない。それは自分の振る舞いについての指示なので
/// 完全には従わせられず、断り書きが 1 行返ってくる (実測)。返る形が決まって
/// いれば、受け取った側がその 1 行を畳んで見せずに済む。空白を含まない形に
/// するのは、受け取った側が 1 つの語として拾えるようにするため。
pub fn marker(nonce: &str) -> String {
    format!(
        "[llm-gateway cache keepalive nonce={nonce}] \
         Reply with exactly this token and nothing else: \
         LLMGW-KEEPALIVE-{nonce}"
    )
}

/// 受け口へ流す 1 件。
///
/// 転送の知らせ ([`Event`]) と cache の合図 ([`Keepalive`]) は別の出来事で、
/// 欄も重ならない。1 つの型に混ぜて空欄で埋めるのではなく、種類として分ける。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Notice {
    Request(Event),
    CacheKeepalive(Keepalive),
}

impl Notice {
    /// SSE の 1 通に付ける名前。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Request(_) => "request",
            Self::CacheKeepalive(_) => Keepalive::KIND,
        }
    }

    /// 転送の知らせなら中身。合図なら `None`。
    pub fn request(&self) -> Option<&Event> {
        match self {
            Self::Request(event) => Some(event),
            Self::CacheKeepalive(_) => None,
        }
    }
}

impl From<Event> for Notice {
    fn from(event: Event) -> Self {
        Self::Request(event)
    }
}

impl From<Keepalive> for Notice {
    fn from(keepalive: Keepalive) -> Self {
        Self::CacheKeepalive(keepalive)
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

    const NOW: i64 = 1_800_000_000;

    /// 素性を 1 つ組む。試験で変えたい欄だけを書けるようにする。
    fn from(credential: &str) -> Origin<'_> {
        Origin {
            session_id: None,
            prefix: None,
            ns: "personal",
            model: "m",
            credential,
            keepalive: None,
        }
    }

    /// 転送の知らせとして届いた 1 件。
    fn request(notice: Notice) -> Event {
        match notice {
            Notice::Request(event) => event,
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
        assert_eq!(got.ts_iso, format_rfc3339(NOW));
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
        assert_eq!(json["ts_iso"], format_rfc3339(NOW));
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
    }
}
