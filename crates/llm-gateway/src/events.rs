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

use crate::cache::keepalive::{Breakeven, Chain};
use crate::denial::Reason;
use crate::metering::{Outcome, TokenKind, TokenUsage};

/// 系列の識別子として出すハッシュの長さ (16 進の桁数)。
///
/// 見分けが付けば足りる。人がログで突き合わせるので、短いほうが読める。
const PREFIX_DIGITS: usize = 8;

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
    /// 連鎖 (`cache_*`) から見れば、ここが「今」になる。
    pub ts: i64,
    /// gateway 全体の通し番号 ([`Events::publish`] が振る)。起動からの連番で、
    /// 最初の 1 件が 1。前に受けた番号 + 1 でなければ、間が欠けている。
    pub seq: u64,
    /// どの起動の番号か ([`Events::boot`])。変わったら `seq` は振り直し。
    pub boot: i64,
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
    /// この 1 本が約束した寿命の id (DR-0012)。
    ///
    /// [`Self::cache_expires_at`] を出す 1 本だけが持つ。約束が果たされずに
    /// 終わったとき、`cache_expired` がこの id を名指しで取り消す。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_notice: Option<String>,
    /// 経路選定で外した経路。無ければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// 連鎖の起点 = この系列で最後に来た実リクエストを送った時刻。
    ///
    /// ここから下の `cache_*` は、控えの付いている系列 (`keepalive` 戦略が
    /// 効く本流) にだけ出る。付いていなければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_since: Option<i64>,
    /// 次の送り直しの予定時刻。次が無ければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_keepalive_at: Option<i64>,
    /// この 1 本が連鎖の何番目か。実リクエストは 0、k 回目の送り直しは k。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_count: Option<u32>,
    /// この系列の cache を、送り直しで継ぎ足せる終わり。
    ///
    /// [`Self::cache_expires_at`] が「この 1 本が置いた cache がいつ消えるか」
    /// なのに対して、こちらは**最後に送る 1 本が置く cache がいつ消えるか**。
    /// 会話が止まったままなら、実際に切れるのはこの時刻になる。
    ///
    /// 送れなかった場合 (経路が塞がる) は、ここより早く切れる。見る側は
    /// 最新の知らせで上書きする。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_until: Option<i64>,
    /// 連鎖で送る総数。[`Self::cache_until`] を作る 1 本の番号でもある。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_until_count: Option<u32>,
    /// 損益分岐時間まで繋いだ場合の終わり (DR-0024 §3)。
    ///
    /// 単価が分からず分岐時間を出せないモデルでは欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_breakeven_until: Option<i64>,
    /// 分岐時間に収まる本数。
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
    /// この 1 本が約束した寿命の id ([`Event::cache_notice`])。
    pub cache_notice: Option<&'a str>,
    /// この系列に立っている連鎖 ([`Event::cache_since`] 以下)。
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
            // 番号は流すときに振る ([`Events::publish`])。
            seq: 0,
            boot: 0,
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
            // 約束していない 1 本に id は要らない。取り消す相手が無い。
            cache_notice: origin
                .cache_ttl_secs
                .and(origin.cache_notice)
                .map(str::to_owned),
            skipped,
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

/// この 1 本で prompt cache が実際にどう働いたか (DR-0012)。
///
/// [`Event::cache_expires_at`] は送る前に**見込み**で出す欄で、上流の都合で
/// cache が消えていたかどうかは、応答の usage を読むまで分からない。見る側
/// (ccmsg) が見込みのまま残りを描くと、消えた cache に「7 割残っている」と
/// いう嘘のリングが出る (実測 2026-09-10)。結果の語だけをここで渡す。
///
/// **数は載せない** — トークン数を出さないのは DR-0012 の決めごとで、
/// 見る側が要るのは「延びたのか、書き直したのか」だけ。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cache {
    /// 置いてあった cache から読めた = 狙いどおり延びた。
    Hit,
    /// 全量を書いた = 繋ぐものが無く、作り直した (1h なら 2 倍単価)。
    Written,
    /// 一部は読めて、残りを書き足した。会話が伸びた分がここに出る。
    Partial,
    /// cache を使わなかった 1 本 (戦略の当たらない経路がこれ)。
    None,
    /// usage が読めなかった。切れた応答・報告しない口がこれ。
    #[default]
    Unknown,
}

impl Cache {
    /// 読めた usage から、この 1 本の結果を決める。
    ///
    /// 見るのは core の区分 ([`crate::metering::TokenKind`]) なので、どの
    /// provider の方言で届いたかに依らない。
    pub fn of(usage: Option<&TokenUsage>) -> Self {
        let Some(usage) = usage.filter(|usage| !usage.is_empty()) else {
            return Self::Unknown;
        };
        let read = usage.get(&TokenKind::input_cache_read()).unwrap_or(0);
        let written = usage
            .get(&TokenKind::input_cache_creation())
            .unwrap_or_default();
        match (read > 0, written > 0) {
            (true, false) => Self::Hit,
            (false, true) => Self::Written,
            (true, true) => Self::Partial,
            (false, false) => Self::None,
        }
    }

    /// この 1 本が prompt cache に乗ったか。
    ///
    /// 乗ったのは、読めた (`hit`)・書いた (`written`)・その両方 (`partial`) の
    /// 3 つ。`none` は cache を使わなかった 1 本で、`unknown` は使ったかどうかが
    /// 分からない 1 本なので、どちらも「乗った」とは言えない。
    pub fn on_cache(self) -> bool {
        matches!(self, Self::Hit | Self::Written | Self::Partial)
    }

    /// 知らせに出す 1 語。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Written => "written",
            Self::Partial => "partial",
            Self::None => "none",
            Self::Unknown => "unknown",
        }
    }
}

/// 応答本文が終わった、という知らせ。
///
/// [`Event`] が「upstream へ送り始めた」を伝えるのに対して、こちらは
/// **その 1 本の応答が閉じた瞬間**を伝える。見る側 (ccmsg) は
/// [`Self::stop_reason`] が `end_turn` の 1 通で「クライアントは入力待ちに
/// 戻った」と判断する — `tool_use` ならクライアントが道具を動かして次の
/// 1 本が来るので、まだ待っていない。
///
/// クライアントが途中で切った場合も入力待ちに戻るので、そこでも 1 通出す
/// ([`Self::aborted`])。
///
/// 対応する [`Event`] とは [`Self::request_ts`] で結ぶ。時刻の欄はすべて
/// Unix ミリ秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// 受け取る側が種類を見分ける印。値は常に `response`。
    #[serde(rename = "type")]
    pub kind: String,
    /// この知らせの時刻 = 本文が閉じた (または切れた) 瞬間。
    pub ts: i64,
    /// gateway 全体の通し番号 ([`Events::publish`] が振る)。起動からの連番で、
    /// 最初の 1 件が 1。前に受けた番号 + 1 でなければ、間が欠けている。
    pub seq: u64,
    /// どの起動の番号か ([`Events::boot`])。変わったら `seq` は振り直し。
    pub boot: i64,
    /// 対応する [`Event`] の [`Event::ts`]。同じ会話で何本も走るので、
    /// 素性が同じでも 1 対 1 に結べるようにする。
    pub request_ts: i64,
    /// どの会話か。ヘッダを付けてこないクライアントでは `null`。
    pub session_id: Option<String>,
    /// この会話系列の識別子 ([`prefix`])。取れなければ欄ごと出さない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// どの namespace 宛か。
    pub ns: String,
    /// 解決後の実モデル名。
    pub model: String,
    /// 答えた経路の名前。
    pub credential: String,
    /// この 1 本を出した側 (`main` / `sub` / `unknown`、DR-0024)。
    pub origin: String,
    /// upstream が返した状態。対応する [`Event`] と同じ値。
    pub status: u16,
    /// upstream が言った終わり方。**値はそのまま写す**。取れなければ
    /// 欄ごと出さない (途中で切れた場合がこれ)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// 本文が最後まで流れなかったか。**常に出す** — 欄が消えると、見る側が
    /// 「切れていない」と区別できない。
    pub aborted: bool,
    /// この 1 本で prompt cache がどう働いたか。**常に出す** — 読めなかった
    /// ことも [`Cache::Unknown`] として伝わる必要がある (欄が消えると、
    /// 見る側は送る前の見込みを信じ続ける)。
    #[serde(default)]
    pub cache: Cache,
}

impl Response {
    /// 種類の印。
    pub const KIND: &'static str = "response";

    /// 素性だけを埋めた下書きを作る。
    ///
    /// 出すのは本文が終わってからだが、素性が揃うのは経路が決まった時点。
    /// 終わり方の分かる場所 ([`crate::exchange`]) は経路も会話も知らないので、
    /// ここで下書きを作って持たせ、終端で [`Self::settle`] が閉じる。
    pub fn pending(request_ts: i64, origin: &Origin<'_>, status: u16) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            // 本文が終わった時刻は、まだ来ていない ([`Self::settle`] が埋める)。
            ts: 0,
            seq: 0,
            boot: 0,
            request_ts,
            session_id: origin.session_id.map(str::to_owned),
            prefix: origin.prefix.map(str::to_owned),
            ns: origin.ns.to_owned(),
            model: origin.model.to_owned(),
            credential: origin.credential.to_owned(),
            origin: origin.origin.to_owned(),
            status,
            stop_reason: None,
            aborted: false,
            cache: Cache::Unknown,
        }
    }

    /// 本文が終わった。時刻と、本文から読めたことを入れて流せる形にする。
    ///
    /// 終わり方も cache の結果も同じ観測 ([`Outcome`]) から出るので、まとめて
    /// 受け取る。
    pub fn settle(&mut self, ts_ms: i64, outcome: &Outcome, aborted: bool) {
        self.ts = ts_ms;
        self.cache = Cache::of(outcome.usage.as_ref());
        self.stop_reason = outcome.stop_reason.clone();
        self.aborted = aborted;
    }
}

/// 無変換の中継 1 本 (DR-0030 §2)。
///
/// 中継には model もトークンも無いので、転送の知らせ ([`Event`]) とは欄が
/// 重ならない。種類として分ける。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passthrough {
    /// 受け取る側が種類を見分ける印。値は常に `passthrough`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 受けた時刻 (Unix ミリ秒)。
    pub ts: i64,
    /// gateway 全体の通し番号 ([`Events::publish`] が振る)。
    pub seq: u64,
    /// どの起動の番号か ([`Events::boot`])。
    pub boot: i64,
    pub ns: String,
    /// 行き先の名前 (`[upstreams.<name>]`)。
    pub upstream: String,
    pub method: String,
    /// `<rest>` のパス。クエリは含めない (秘密や個人の値が載りうるため)。
    pub path: String,
    /// クライアントへ返した状態コード。gateway が断った場合 (404 / 405 / 502) も入る。
    pub status: u16,
    /// 受けてから応答のヘッダが返るまで (ミリ秒)。
    pub duration_ms: u64,
    /// 載せた固定の秘密の識別子。未登録の行き先では空。
    pub secret: String,
    /// gateway が断った理由。上流まで届いたものは無い (上流が返した 4xx / 5xx も無い)。
    ///
    /// `unknown_upstream` / `unsafe_path` / `ns_allow` / `upstream_allow` /
    /// `secret` / `unreachable`。状態コードだけでは、404 が namespace の側か
    /// 行き先の側か読めない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// `rate_limited` のとき、埋まっていたバケット (`minute` / `hour` 等)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// `rate_limited` のとき、クライアントへ返した `Retry-After` (秒)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

impl Passthrough {
    /// 種類の印。
    pub const KIND: &'static str = "passthrough";
}

/// 約束した寿命が果たされずに終わった、という取り消し (DR-0012)。
///
/// `cache_expires_at` は送る前に立てた見込みで、上流の都合や機械のサスペンドで
/// cache が先に消えることがある。見る側 (ccmsg) は最後に受けた約束の残りを
/// 描き続けるので、消えたことを伝える口がないと嘘のまま残る。
///
/// 取り消すのは [`Self::of`] が指す**その約束 1 つ**だけ。受け取る側は
/// (会話, 系列) ごとに最後の `cache_notice` を覚えておき、一致したときだけ
/// 残りを 0 にする。一致しなければ、その約束は既に新しいもので置き換わって
/// いる (別の gateway が先に延ばした等) ので、何もしない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheExpired {
    /// 受け取る側が種類を見分ける印。値は常に `cache_expired`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 消えたと分かった時刻 (Unix ミリ秒)。
    pub ts: i64,
    /// gateway 全体の通し番号 ([`Events::publish`] が振る)。起動からの連番で、
    /// 最初の 1 件が 1。前に受けた番号 + 1 でなければ、間が欠けている。
    pub seq: u64,
    /// どの起動の番号か ([`Events::boot`])。変わったら `seq` は振り直し。
    pub boot: i64,
    /// どの会話か。
    pub session_id: String,
    /// その会話のどの系列か ([`prefix`])。
    pub prefix: String,
    /// 取り消す約束の id ([`Event::cache_notice`])。
    pub of: String,
}

impl CacheExpired {
    /// 種類の印。
    pub const KIND: &'static str = "cache_expired";

    pub fn new(ts_ms: i64, session_id: &str, prefix: &str, of: &str) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            ts: ts_ms,
            seq: 0,
            boot: 0,
            session_id: session_id.to_owned(),
            prefix: prefix.to_owned(),
            of: of.to_owned(),
        }
    }
}

/// 受け口へ流す 1 件。
///
/// 転送の知らせ ([`Event`]) と約束の取り消し ([`CacheExpired`]) は別の出来事で、
/// 欄も重ならない。1 つの型に混ぜて空欄で埋めるのではなく、種類として分ける。
///
/// 読み戻すときは書いた順に当てはめる (`untagged`)。印を持つ種類を先に置くのは、
/// 印の無い転送の知らせが**他の種類まで飲み込まないようにする**ため。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Notice {
    /// 約束した寿命の取り消し。
    CacheExpired(CacheExpired),
    /// 応答が閉じた知らせ。
    Response(Box<Response>),
    /// 無変換の中継。
    Passthrough(Box<Passthrough>),
    /// 転送の知らせ。欄が多く、他の 3 種より大きいので箱に入れる
    /// (この列は 1 件流すたびに写される)。
    Request(Box<Event>),
}

impl Notice {
    /// SSE の 1 通に付ける名前。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Request(_) => "request",
            Self::Response(_) => Response::KIND,
            Self::CacheExpired(_) => CacheExpired::KIND,
            Self::Passthrough(_) => Passthrough::KIND,
        }
    }

    /// gateway 全体の通し番号 ([`Events::publish`] が振った値)。
    pub fn seq(&self) -> u64 {
        match self {
            Self::Request(event) => event.seq,
            Self::Response(response) => response.seq,
            Self::CacheExpired(expired) => expired.seq,
            Self::Passthrough(relayed) => relayed.seq,
        }
    }

    /// 転送の知らせなら中身。それ以外なら `None`。
    pub fn request(&self) -> Option<&Event> {
        match self {
            Self::Request(event) => Some(event),
            Self::Response(_) | Self::CacheExpired(_) | Self::Passthrough(_) => None,
        }
    }

    /// 応答が閉じた知らせなら中身。それ以外なら `None`。
    pub fn response(&self) -> Option<&Response> {
        match self {
            Self::Response(response) => Some(response),
            Self::Request(_) | Self::CacheExpired(_) | Self::Passthrough(_) => None,
        }
    }
}

impl gateway_core::events::Stamped for Notice {
    /// 通し番号と起動の印を押す。押すのは流す 1 箇所 ([`Events::publish`]) だけ。
    fn stamp(&mut self, seq: u64, boot: i64) {
        let (to_seq, to_boot) = match self {
            Self::Request(event) => (&mut event.seq, &mut event.boot),
            Self::Response(response) => (&mut response.seq, &mut response.boot),
            Self::CacheExpired(expired) => (&mut expired.seq, &mut expired.boot),
            Self::Passthrough(relayed) => (&mut relayed.seq, &mut relayed.boot),
        };
        *to_seq = seq;
        *to_boot = boot;
    }
}

impl From<Event> for Notice {
    fn from(event: Event) -> Self {
        Self::Request(Box::new(event))
    }
}

impl From<Response> for Notice {
    fn from(response: Response) -> Self {
        Self::Response(Box::new(response))
    }
}

impl From<Passthrough> for Notice {
    fn from(relayed: Passthrough) -> Self {
        Self::Passthrough(Box::new(relayed))
    }
}

impl From<CacheExpired> for Notice {
    fn from(expired: CacheExpired) -> Self {
        Self::CacheExpired(expired)
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

/// 見ている人へ配る口。仕組みは汎用層が持つ。
pub type Events = gateway_core::events::Events<Notice>;

/// 1 人ぶんの見る口。
pub type Watching = gateway_core::events::Watching<Notice>;

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
            chain: None,
            breakeven: None,
            origin: "main",
            cache_ttl_secs: None,
            cache_notice: None,
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
    /// 流した順に 1 から番号が振られ、種類をまたいでも同じ列で数える。
    #[tokio::test]
    async fn every_notice_gets_the_next_number() {
        let events = Events::new();
        let mut watching = events.subscribe();
        events.publish(Event::new(NOW, &from("a"), 200));
        events.publish(CacheExpired::new(NOW, "s-1", "2cf24dba", "n-1"));
        events.publish(Event::new(NOW, &from("b"), 200));

        let mut seqs = Vec::new();
        for _ in 0..3 {
            let got = watching.recv().await.unwrap();
            assert_eq!(serde_json::to_value(&got).unwrap()["boot"], events.boot());
            seqs.push(got.seq());
        }
        assert_eq!(seqs, [1, 2, 3]);
    }

    /// 遅れて落とした見る側は、次に受け取る番号の飛びで欠けたと分かる。
    #[tokio::test]
    async fn a_gap_in_the_numbers_shows_what_was_dropped() {
        let events = Events::new();
        let mut watching = events.subscribe();
        let total = gateway_core::events::BACKLOG as u64 + 10;
        for _ in 0..total {
            events.publish(Event::new(NOW, &from("a"), 200));
        }

        let first = loop {
            match watching.recv().await {
                Ok(notice) => break notice,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("{e}"),
            }
        };
        assert_eq!(
            first.seq(),
            total - gateway_core::events::BACKLOG as u64 + 1
        );
        assert_eq!(
            first.seq() - 1,
            events.dropped(),
            "the gap is what was dropped"
        );
    }

    /// 中継の知らせは読み戻しても中継の種類のまま (転送の知らせに飲まれない)。
    #[test]
    fn a_passthrough_notice_reads_back_as_itself() {
        let notice = Notice::from(Passthrough {
            kind: Passthrough::KIND.to_owned(),
            ts: NOW,
            seq: 3,
            boot: 1,
            ns: "default".into(),
            upstream: "api.example.test".into(),
            method: "GET".into(),
            path: "/v1/items".into(),
            status: 200,
            duration_ms: 12,
            secret: "ex".into(),
            refused: Some("rate_limited".into()),
            bucket: Some("minute".into()),
            retry_after_secs: Some(30),
        });
        let text = serde_json::to_string(&notice).unwrap();
        let back: Notice = serde_json::from_str(&text).unwrap();
        assert_eq!(back, notice);
        assert_eq!(back.name(), "passthrough");
        assert_eq!(back.seq(), 3);
        assert!(back.request().is_none() && back.response().is_none());
    }

    /// 番号は JSON で時刻の隣に出る。
    #[test]
    fn the_number_sits_next_to_the_time() {
        let events = Events::new();
        let mut watching = events.subscribe();
        events.publish(Event::new(NOW, &from("a"), 200));
        let sent = serde_json::to_string(&watching.try_recv().unwrap()).unwrap();
        let head = format!(r#"{{"ts":{NOW},"seq":1,"boot":{},"#, events.boot());
        assert!(sent.starts_with(&head), "{sent}");
    }

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

        let quiet = serde_json::to_value(Event::new(NOW, &from("a"), 200)).unwrap();
        for field in [
            "cache_since",
            "next_keepalive_at",
            "cache_count",
            "cache_until",
            "cache_until_count",
        ] {
            assert!(
                quiet.get(field).is_none(),
                "{field} is omitted when nothing is kept for this series"
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
                cache_notice: Some("n0tice"),
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
                // まだ流していない 1 件は番号を持たない ([`Events::publish`] が振る)。
                "seq": 0,
                "boot": 0,
                "session_id": "s-1",
                "ns": "personal",
                "model": "claude-opus-5",
                "credential": "personal",
                "status": 200,
                "prefix": "2cf24dba",
                "origin": "main",
                "cache_ttl_secs": 3600,
                "cache_expires_at": NOW + HOUR,
                "cache_notice": "n0tice",
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
                "seq": 0,
                "boot": 0,
                "session_id": "s-1",
                "ns": "personal",
                "model": "claude-opus-5",
                "credential": "personal",
                "status": 200,
                "prefix": "2cf24dba",
                "origin": "main",
                "cache_ttl_secs": 3600,
                "cache_expires_at": NOW + 55 * MINUTE + HOUR,
                "cache_since": NOW,
                "next_keepalive_at": NOW + 2 * 55 * MINUTE,
                "cache_count": 1,
                "cache_until": NOW + 9 * 55 * MINUTE + HOUR,
                "cache_until_count": 9,
            }),
            "a replay of the same series moves the chain along"
        );

        let withdrawn = Notice::from(CacheExpired::new(NOW + HOUR, "s-1", "2cf24dba", "n0tice"));
        assert_eq!(
            serde_json::to_value(&withdrawn).unwrap(),
            json!({
                "type": "cache_expired",
                "ts": NOW + HOUR,
                "seq": 0,
                "boot": 0,
                "session_id": "s-1",
                "prefix": "2cf24dba",
                "of": "n0tice",
            })
        );
        assert_eq!(withdrawn.name(), "cache_expired");
        assert_eq!(
            serde_json::from_value::<Notice>(serde_json::to_value(&withdrawn).unwrap()).unwrap(),
            withdrawn,
            "a withdrawal reads back as itself, not as another kind of notice"
        );
        assert!(
            withdrawn.request().is_none() && withdrawn.response().is_none(),
            "it is neither a forward nor a completion"
        );
    }

    /// cache から読んだだけの usage。
    fn read(tokens: u64) -> TokenUsage {
        let mut usage = TokenUsage::default();
        usage.set(TokenKind::input_cache_read(), tokens);
        usage
    }

    /// upstream が返した usage の形が、そのまま cache の結果になる。
    ///
    /// 見る側 (ccmsg) は送る前の見込み (`cache_expires_at`) をこの語で
    /// 上書きするので、4 通りの読み分けが契約そのものになる。
    #[test]
    fn the_usage_says_how_the_cache_worked() {
        let usage = |read: u64, written: u64| {
            let mut usage = TokenUsage::default();
            usage.set(TokenKind::input_cache_read(), read);
            usage.set(TokenKind::input_cache_creation(), written);
            usage.set(TokenKind::input(), 12);
            Some(usage)
        };

        // 読めた = 狙いどおり延びた。書いた = 繋ぐものが無く作り直した。
        assert_eq!(Cache::of(usage(3_000, 0).as_ref()), Cache::Hit);
        assert_eq!(Cache::of(usage(0, 3_000).as_ref()), Cache::Written);
        // 会話が伸びた分を書き足した 1 本は、どちらでもない。
        assert_eq!(Cache::of(usage(3_000, 40).as_ref()), Cache::Partial);
        // usage は読めたが、cache には触っていない 1 本。
        assert_eq!(Cache::of(usage(0, 0).as_ref()), Cache::None);
        // 読めなかったことは、当てずっぽうを返さずにそのまま伝える。
        assert_eq!(Cache::of(None), Cache::Unknown);
        assert_eq!(Cache::of(Some(&TokenUsage::default())), Cache::Unknown);

        // 語は見る側との契約。綴りを変えると読めなくなる。
        assert_eq!(
            [
                Cache::Hit,
                Cache::Written,
                Cache::Partial,
                Cache::None,
                Cache::Unknown
            ]
            .map(Cache::as_str),
            ["hit", "written", "partial", "none", "unknown"]
        );
        assert_eq!(
            serde_json::to_value(Cache::Written).unwrap(),
            "written",
            "the word goes out as it is spelled"
        );
    }

    /// 数は載せない (DR-0012)。出すのは結果の語だけ。
    #[test]
    fn the_completion_notice_carries_no_token_counts() {
        let mut notice = Response::pending(NOW, &from("personal"), 200);
        notice.settle(
            NOW + 1,
            &Outcome {
                usage: Some(read(123_456)),
                stop_reason: Some("end_turn".to_owned()),
            },
            false,
        );

        let json = serde_json::to_value(&notice).unwrap();
        assert_eq!(json["cache"], "hit");
        assert!(
            !json.to_string().contains("123456"),
            "the token counts stay out of the notice: {json}"
        );
        assert!(
            json.as_object()
                .unwrap()
                .keys()
                .all(|key| !key.contains("token")),
            "no token field at all: {json}"
        );
    }

    /// 応答が閉じた知らせの全文。欄の名前・並び・単位が、見る側との契約になる。
    #[test]
    fn a_completion_notice_is_settled() {
        let mut notice = Response::pending(
            NOW,
            &Origin {
                session_id: Some("s-1"),
                prefix: Some("2cf24dba"),
                model: "claude-opus-5",
                origin: "main",
                ..from("personal")
            },
            200,
        );
        notice.settle(
            NOW + 8 * 1_000,
            &Outcome {
                usage: Some(read(3_000)),
                stop_reason: Some("end_turn".to_owned()),
            },
            false,
        );

        assert_eq!(
            serde_json::to_value(&notice).unwrap(),
            json!({
                "type": "response",
                "ts": NOW + 8 * 1_000,
                // まだ流していない 1 件は番号を持たない ([`Events::publish`] が振る)。
                "seq": 0,
                "boot": 0,
                "request_ts": NOW,
                "session_id": "s-1",
                "prefix": "2cf24dba",
                "ns": "personal",
                "model": "claude-opus-5",
                "credential": "personal",
                "origin": "main",
                "status": 200,
                "stop_reason": "end_turn",
                "aborted": false,
                "cache": "hit",
            })
        );

        // 切れた 1 本には終わり方が無い。切れたことは常に出す。
        let mut cut = Response::pending(NOW, &from("personal"), 200);
        cut.settle(NOW + 3 * 1_000, &Outcome::default(), true);
        assert_eq!(
            serde_json::to_value(&cut).unwrap()["cache"],
            "unknown",
            "a body that never reported its usage says so, rather than going quiet"
        );
        let json = serde_json::to_value(&cut).unwrap();
        assert_eq!(json["aborted"], true);
        assert!(
            json.get("stop_reason").is_none(),
            "omitted when the upstream never said how it ended"
        );
        assert!(
            json.get("prefix").is_none(),
            "omits the field entirely when the series is unknown"
        );
        assert!(
            json["session_id"].is_null(),
            "the field stays even when the conversation is unknown"
        );
        assert!(
            json.as_object()
                .unwrap()
                .keys()
                .all(|key| !key.ends_with("_iso")),
            "times are one number each: {json}"
        );
    }

    /// 応答の知らせは、転送の知らせと取り違えられない。
    ///
    /// 受け取る側は 1 つの列で読むので、型を跨いで当てはまると欄が化ける。
    #[test]
    fn a_completion_is_told_apart_from_a_forward() {
        let mut response = Response::pending(NOW, &from("a"), 200);
        response.settle(
            NOW + 1,
            &Outcome {
                usage: None,
                stop_reason: Some("end_turn".to_owned()),
            },
            false,
        );
        let notice = Notice::from(response);

        assert_eq!(notice.name(), "response");
        assert!(notice.request().is_none(), "it is not a forwarding notice");
        assert_eq!(
            notice.response().map(|r| r.stop_reason.as_deref()),
            Some(Some("end_turn"))
        );

        let json = serde_json::to_value(&notice).unwrap();
        assert_eq!(
            serde_json::from_value::<Notice>(json).unwrap(),
            notice,
            "it reads back as the same kind of notice"
        );

        // 逆向きも同じ。転送の知らせが応答の知らせに化けない。
        let forwarded = Notice::from(Event::new(NOW, &from("a"), 200));
        assert!(forwarded.response().is_none());
        assert_eq!(
            serde_json::from_value::<Notice>(serde_json::to_value(&forwarded).unwrap()).unwrap(),
            forwarded
        );
    }
}
