//! 止まった会話の cache を、gateway 自身の送り直しで繋ぐ (DR-0027)。
//!
//! 1 時間の cache は、会話が止まった 1 時間後に消える。消えてから再開すると
//! その時点のプレフィックス全量を書き直すことになり、書き込みは読み出しの
//! 50 倍の単価で効く。そこで**消える手前で 1 本だけ送り直す**。プレフィックスに
//! 当たった 1 本はそのエントリの TTL を更新するので (`docs/findings/
//! 2026-09-08-cache-ttl-refresh-on-hit.md`)、繋ぐのに新しい内容も、会話の
//! 相手も要らない。
//!
//! 送るのは**最後に upstream へ転送した本文そのもの** ([`store`])。加工しないので
//! プレフィックスが必ず一致する。変えるのは [`max_tokens`][MAX_TOKENS_FIELD] を
//! 1 にすることだけで、`stream` にも `thinking` にも触らない — `max_tokens` は
//! cache key に入らず、読み捨ての費用は output 1 トークンで済む (DR-0027 決定 1)。
//! 応答は読み捨てるが、usage だけは読む: 繋がったのか (`hit`)、既に消えていて
//! 書き直したのか (`written`) は、そこにしか出ない。
//!
//! 送る前に**壁時計**で cache の期限と期間の終わりを見る。予定を運ぶ単調時計は
//! 機械が眠っている間止まる (macOS のサスペンド) ので、復帰の直後は「予定どおり
//! の時刻」に見えても、壁時計では繋ぐ相手がとうに消えている。
//!
//! 兄弟 (11301 / 11302) は同じ置き場を共有し、送る直前に系列の `.lock` を掴む。
//! 掴めなければ相手が撫でているので、こちらは黙って次へ回る。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;

use crate::credential::time::now_unix_ms;
use crate::egress::{BoxFuture, RequestShape};
use crate::events::{self, Events};

pub mod store;
pub mod uncached;

pub use store::{Kept, Series};
pub use uncached::{Dropped, Evidence, Uncached};

/// 1 本送ってから、次に送り直すまでの時間。
///
/// cache が消える手前。会話が動いている間は実リクエストのたびに先送りされる
/// ので、ここまで空くこと自体が「止まった」の合図になる。
const REFRESH_AFTER: Duration = Duration::from_secs(55 * 60);

/// 送った本文が残す cache の寿命 (`keepalive` は全ブレークポイントが 1 時間)。
const LIFETIME: Duration = Duration::from_secs(60 * 60);

/// 期限にどれだけ余裕を見るか。
///
/// 送ってから upstream が前処理を始めるまでの分。切り詰めると、間に合った
/// つもりの 1 本が全量の書き直しになる。
const MARGIN: Duration = Duration::from_secs(30);

/// 送り直しで 1 にする欄。ここだけが元の本文と変わる。
const MAX_TOKENS_FIELD: &str = "max_tokens";

/// 約束 1 つの名前に使う乱数の長さ (バイト)。
const NOTICE_BYTES: usize = 16;

/// 直前の実リクエストが通った先。
///
/// 送り直す前に、そこがまだ使えるかを確かめるために覚えておく。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub ns: String,
    pub model: String,
    pub route: String,
}

/// 経路が今も使えるかを答える口。
///
/// 締め出しや候補の入れ替わりを知っているのは経路を選ぶ側なので、こちらは
/// 答えだけを聞く。
pub trait Reachable: Send + Sync {
    fn usable<'a>(&'a self, bound: &'a Bound) -> BoxFuture<'a, bool>;
}

/// この系列に立っている送り直しの連鎖の見立て (時刻は Unix ミリ秒)。
///
/// 見る側 (ccmsg) が「この会話の cache はいつまで、あと何本で保つか」を
/// 描くための一式 (DR-0012 の `cache_*`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chain {
    /// 連鎖の起点 = 最後に来た実リクエストの時刻 (Unix ミリ秒)。
    pub since_ms: i64,
    /// ここまでに送り直した本数。実リクエストの直後は 0。
    pub count: u32,
    /// 次の送り直しの予定時刻 (Unix ミリ秒)。もう送らないなら `None`。
    pub next_at_ms: Option<i64>,
    /// 最後の 1 本が置く cache が消える時刻 (Unix ミリ秒)。
    pub until_ms: i64,
    /// 連鎖で送る総数。[`Self::until_ms`] を作る 1 本の番号でもある。
    pub until_count: u32,
}

/// 損益分岐時間から起こした連鎖 (DR-0024 §3)。
///
/// 「送り直し続ける費用が cache の作り直しに追いつく」までに何本送れて、
/// そこまで繋いだ cache がいつ切れるか。実際に送る本数 ([`Chain::until_count`])
/// と並べると、設定した期間が分岐点の手前か先かが読める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakeven {
    /// 分岐時間に収まる本数。
    pub count: u32,
    /// その最後の 1 本が置く cache が消える時刻 (Unix ミリ秒)。
    pub until_ms: i64,
}

impl Chain {
    /// この 1 本が置く控えから見た連鎖。
    ///
    /// 実リクエストは控えを置き直して連鎖を 0 から数え直すので、送る時点で
    /// 全部決まる ([`Keepalive::armed_by_request`] が書く値と同じ)。知らせに
    /// 出すのは**この見立て**で、控えが実際に置かれるのは応答を読み切った後
    /// (DR-0027 決定 8) — 乗らなかったときは `cache_expired` が取り消す。
    pub fn promised(since_ms: i64, horizon: Duration) -> Self {
        Self::laid_out(
            since_ms,
            0,
            since_ms + refresh_ms(),
            since_ms + horizon.as_millis() as i64,
        )
    }

    /// 起点・本数・次の予定・期間の終わりから、55 分の格子に並べる。
    fn laid_out(since_ms: i64, count: u32, fires_at_ms: i64, horizon_end_ms: i64) -> Self {
        let at = |nth: u32| since_ms + i64::from(nth) * refresh_ms();
        // 期間の終わりを跨いだ 1 本まで出る (発火時点で期間が残っていれば
        // 次を仕込む)。次の 1 本の後に、あと何本続くか。
        let more =
            ((horizon_end_ms - fires_at_ms).max(0) as u64).div_ceil(refresh_ms() as u64) as u32;
        let until_count = count + 1 + more;
        Self {
            since_ms,
            count,
            next_at_ms: Some(at(count + 1)),
            until_ms: at(until_count) + LIFETIME.as_millis() as i64,
            until_count,
        }
    }
}

impl Breakeven {
    /// この起点から数えた分岐点。本数の数え方は実際の連鎖と同じ
    /// ([`signals_within`])。
    pub fn from(since_ms: i64, breakeven: Duration) -> Self {
        let count = signals_within(breakeven);
        Self {
            count,
            until_ms: since_ms + (REFRESH_AFTER * count + LIFETIME).as_millis() as i64,
        }
    }
}

/// この長さの期間に送る本数。
///
/// 期間の終わりを跨いだ 1 本まで送る ([`Keepalive::chain`] と同じ規則)。
/// 期間が [`REFRESH_AFTER`] に満たなくても 1 本は送る — 実リクエストは期間を
/// 見ずに最初の予定を置く ([`Keepalive::armed_by_request`])。
pub fn signals_within(horizon: Duration) -> u32 {
    horizon
        .as_secs()
        .div_ceil(REFRESH_AFTER.as_secs())
        .max(1)
        .min(u32::MAX as u64) as u32
}

/// この 1 本が会話の本流か。
///
/// 道具を渡していないリクエスト (分類器・要約など) は、本流とは別の
/// プレフィックスで走る。そこを控えて送り直しても、延ばしたい cache は
/// 延びない (DR-0024 §2 の横断条件)。
pub fn carries_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

/// 約束 1 つの名前 (`cache_notice`、DR-0012)。16 バイトの乱数を base64url に
/// した 22 文字。
///
/// 取り消し (`cache_expired`) が名指しで指すためだけの id なので、中身は持た
/// せない。当てられると他所の約束を取り消させられるので、推測できない乱数から
/// 作る。
pub fn notice_id() -> String {
    let mut bytes = [0u8; NOTICE_BYTES];
    rand::fill(&mut bytes);
    B64URL.encode(bytes)
}

/// 控えた 1 本を実際に送る役。
///
/// 経路を選び直して認証を付けるのは転送側の仕事なので、こちらは頼むだけ
/// (DR-0014 §3 と同じ切り方)。
pub trait Sender: Send + Sync {
    /// この本文を upstream へ送り、cache がどうなったかを返す。
    fn send<'a>(&'a self, kept: &'a Kept) -> BoxFuture<'a, Outcome>;
}

/// 送り直した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 送れた。応答から読んだ判定の材料が入る。
    Sent(Evidence),
    /// 送れなかった (経路が塞がっていた・転送に失敗した)。次の予定へ回す。
    Unsent,
}

/// 送り直しの仕掛け。
pub struct Keepalive {
    store: store::Store,
    /// 乗らなかった 1 本を研究用に取っておく先。
    quarantine: uncached::Quarantine,
    events: Arc<Events>,
    /// 直前に通った経路がまだ使えるかを聞く先。
    reach: Arc<dyn Reachable>,
    /// 実際に送る役。転送側が自分を渡す。
    ///
    /// 弱い参照で持つのは、転送側が[`Keepalive`]を持つため — 強い参照だと輪に
    /// なって、どちらも落ちない。
    sender: Mutex<Weak<dyn Sender>>,
    /// 系列ごとの次の予定。落ちれば予定も止まる。
    timers: Mutex<HashMap<Series, Timer>>,
}

/// 予定の実体。畳まれたら止まる。
struct Timer(JoinHandle<()>);

impl Drop for Timer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 実リクエスト 1 本から、控えに要るもの。
///
/// 欄が多いので、位置で取り違えないよう名前で渡す。
pub struct Sent {
    pub series: Series,
    pub ns: String,
    pub model: String,
    pub route: String,
    /// 送出直前の本文 (認証差し替え・モデル名書き換え後)。
    pub body: Value,
    /// 送出直前のヘッダ。認証は含めない (送るときに付け直す)。
    pub headers: Vec<(String, String)>,
    pub path: String,
    pub query: Option<String>,
    pub shape: RequestShape,
    /// この 1 本を送った時刻 = 連鎖の起点 (Unix ミリ秒)。
    pub sent_at_ms: i64,
    /// 送り直し続ける期間の長さ。
    pub horizon: Duration,
    /// この 1 本が約束した寿命の id (`cache_notice`、DR-0012)。
    pub cache_notice: Option<String>,
}

impl Sent {
    /// この 1 本から、置く控えを作る。
    ///
    /// 予定の時刻はここで決まる。控えずに終わった 1 本 ([`Keepalive::not_landed`])
    /// でも同じ形にするのは、乗っていれば置いたはずのものを、そのまま研究用の
    /// 退避に移せるようにするため。
    fn into_kept(self) -> Kept {
        Kept {
            session_id: self.series.session_id,
            prefix: self.series.prefix,
            ns: self.ns,
            model: self.model,
            route: self.route,
            body: self.body,
            headers: self.headers,
            path: self.path,
            query: self.query,
            shape: self.shape,
            fires_at_ms: self.sent_at_ms + refresh_ms(),
            expires_at_ms: self.sent_at_ms + (LIFETIME - MARGIN).as_millis() as i64,
            horizon_end_ms: self.sent_at_ms + self.horizon.as_millis() as i64,
            since_ms: self.sent_at_ms,
            count: 0,
            cache_notice: self.cache_notice,
        }
    }
}

impl Keepalive {
    pub fn new(
        dir: impl AsRef<std::path::Path>,
        events: Arc<Events>,
        reach: Arc<dyn Reachable>,
        uncached: uncached::Limits,
    ) -> Self {
        Self {
            store: store::Store::new(&dir),
            quarantine: uncached::Quarantine::new(dir, uncached),
            events,
            reach,
            sender: Mutex::new(Weak::<NoSender>::new()),
            timers: Mutex::new(HashMap::new()),
        }
    }

    /// 実際に送る役を渡す。転送側が組み上がった後で 1 回だけ呼ぶ。
    pub fn served_by(&self, sender: Weak<dyn Sender>) {
        *self.sender.lock().unwrap() = sender;
    }

    /// 実リクエストを送った。控えを置き直して、次の予定を張り直す。
    ///
    /// 前の予定は捨てる。**次のリクエストが来るたびに先送りされる**ので、
    /// 会話が動いている間は一度も発火しない。期間と通った先を延ばせるのは、
    /// この 1 本だけ (DR-0024 §2 の debounce をそのまま引き継ぐ)。
    pub fn armed_by_request(self: &Arc<Self>, sent: Sent) {
        let series = sent.series.clone();
        let kept = sent.into_kept();
        if !self.store.save(&kept) {
            // 置けなかった系列は繋げない。前の予定まで残すと、控えの無い
            // 系列を撫でに行くだけになる。
            self.forget(&series);
            return;
        }
        self.plan(series, REFRESH_AFTER, kept.fires_at_ms);
    }

    /// この系列の控えと予定を捨てる。
    ///
    /// 呼ぶのは、繋ぐ相手が無くなったとき (期限切れ・期間の終わり) と、
    /// 人が止めたとき ([`Self::pause`])。
    pub fn forget(&self, series: &Series) {
        self.timers.lock().unwrap().remove(series);
        self.store.remove(series);
    }

    /// この会話への送り直しを止める (DR-0024 §2 の pause API)。
    ///
    /// 会話の id を持つ**全系列**の控えを落とす。解除は実リクエスト 1 本で
    /// 自動 — 控えが無い系列へ来た 1 本は、そのまま新しい控えを置く
    /// ([`Self::armed_by_request`])。止めたことを覚えておく必要は無い
    /// 兄弟へ渡す停止の一覧は要らない — 置き場が 1 つなので、落ちた控えは
    /// 兄弟からも消えている。
    pub fn pause(&self, session_id: &str) {
        let series: Vec<Series> = self
            .store
            .load_all()
            .into_iter()
            .filter(|kept| kept.session_id == session_id)
            .map(|kept| Series {
                session_id: kept.session_id,
                prefix: kept.prefix,
            })
            .collect();
        for series in &series {
            self.forget(series);
        }
        // 兄弟が持っている予定は、控えが消えたことに発火時点で気づいて畳む。
        self.timers
            .lock()
            .unwrap()
            .retain(|series, _| series.session_id != session_id);
    }

    /// 前回の控えを読み戻して、予定を張り直す (DR-0027 決定 3)。
    ///
    /// 予定の時刻がまだ来ていなければ残りの時間で、過ぎていても cache が
    /// 生きている間なら**すぐに**送る。cache が消えた後・期間の終わった系列は
    /// 捨てる — 送っても繋ぐものが無い。
    pub fn restore(self: &Arc<Self>) {
        let now_ms = now_unix_ms();
        // 落ちている間に古びた退避を、読み戻しのついでに切り詰める。
        self.quarantine.prune(now_ms);
        let mut restored = 0;
        for kept in self.store.load_all() {
            let series = Series {
                session_id: kept.session_id.clone(),
                prefix: kept.prefix.clone(),
            };
            if kept.expires_at_ms <= now_ms || kept.horizon_end_ms <= now_ms {
                // 落ちている間に cache が消えていたら、見る側はまだその寿命を
                // 描いている。期間が終わっただけの系列では黙る — 最後に送った
                // 1 本が置いた cache はまだ生きている。
                if kept.expires_at_ms <= now_ms {
                    self.expired(&series, kept.cache_notice.as_deref());
                }
                self.forget(&series);
                continue;
            }
            let after = Duration::from_millis((kept.fires_at_ms - now_ms).max(0) as u64);
            debug!(
                session = %series.session_id,
                prefix = %series.prefix,
                seconds = after.as_secs(),
                "restoring a cache replay"
            );
            self.plan(series, after, kept.fires_at_ms);
            restored += 1;
        }
        if restored > 0 {
            info!(
                series = restored,
                "picked up the kept conversations to replay"
            );
        }
    }

    /// この 1 本が約束する寿命に名前を付ける (`cache_notice`、DR-0012)。
    pub fn promise(&self) -> String {
        notice_id()
    }

    /// この系列に立っている送り直しの連鎖 (DR-0012 の `cache_*`)。
    ///
    /// **見込み値**。経路が塞がって送れなければ、ここより早く切れる。
    pub fn chain(&self, series: &Series) -> Option<Chain> {
        let kept = self.store.load(series)?;
        Some(Chain::laid_out(
            kept.since_ms,
            kept.count,
            kept.fires_at_ms,
            kept.horizon_end_ms,
        ))
    }

    /// 予定を 1 つ置く。
    ///
    /// `expected_ms` は、この予定を置いた時点で控えに書いてある発火時刻。
    /// 起きたときに控えがそれより先を指していたら、**兄弟が先に送った**
    /// ということ ([`Self::fire`])。壁時計と突き合わせないのは、機械が眠って
    /// いた後に「まだ先だ」と読み違えないため — 比べるのは控えに書いてある値
    /// 同士で、どちらも同じ書き方の時刻になる。
    fn plan(self: &Arc<Self>, series: Series, after: Duration, expected_ms: i64) {
        let fires_at = Instant::now() + after;
        let waking = Arc::clone(self);
        let ringing = series.clone();
        let timer = Timer(tokio::spawn(async move {
            tokio::time::sleep_until(fires_at).await;
            waking.fire(ringing, expected_ms).await;
        }));
        // 前の予定は差し替えで畳まれる (`Timer` の Drop が止める)。
        self.timers.lock().unwrap().insert(series, timer);
    }

    /// 1 本送り直す。
    async fn fire(self: &Arc<Self>, series: Series, expected_ms: i64) {
        // 撫でるのは 1 台だけ。兄弟が掴んでいるなら、その系列はそちらが
        // 繋いでいる — こちらは予定を置き直して次に備える。
        let Some(_claim) = self.store.claim(&series) else {
            debug!(
                session = %series.session_id,
                prefix = %series.prefix,
                "a sibling is replaying this series; standing back"
            );
            self.plan(series, REFRESH_AFTER, expected_ms + refresh_ms());
            return;
        };
        // 掴んでから読み直す。掴めなかった間に兄弟が送っていれば、予定は
        // 先へ動いている。
        let Some(kept) = self.store.load(&series) else {
            self.timers.lock().unwrap().remove(&series);
            return;
        };

        let now_ms = now_unix_ms();
        // 繋ぐ相手が残っているかは**壁時計**で見る。
        let gone = if kept.expires_at_ms <= now_ms {
            Some(("the cache it was extending has expired", true))
        } else if kept.horizon_end_ms <= now_ms {
            Some(("the horizon has passed", false))
        } else {
            None
        };
        if let Some((why, withdraw)) = gone {
            debug!(
                session = %series.session_id,
                prefix = %series.prefix,
                "{why}; dropping the kept conversation instead of replaying"
            );
            if withdraw {
                self.expired(&series, kept.cache_notice.as_deref());
            }
            self.forget(&series);
            return;
        }
        if kept.fires_at_ms > expected_ms {
            // 兄弟が先に送っていた。その予定に合わせて下がる。
            let ahead = kept.fires_at_ms;
            self.plan(
                series,
                Duration::from_millis((ahead - expected_ms) as u64),
                ahead,
            );
            return;
        }

        let bound = Bound {
            ns: kept.ns.clone(),
            model: kept.model.clone(),
            route: kept.route.clone(),
        };
        if !self.reach.usable(&bound).await {
            debug!(
                session = %series.session_id,
                prefix = %series.prefix,
                route = %bound.route,
                "the route this conversation was cached on is unavailable; not replaying"
            );
            self.postpone(series, kept, expected_ms);
            return;
        }
        let Some(sender) = self.sender.lock().unwrap().upgrade() else {
            return;
        };

        let outcome = sender.send(&kept).await;
        let sent_at_ms = now_unix_ms();
        match outcome {
            Outcome::Unsent => self.postpone(series, kept, expected_ms),
            // 乗らなかった (`none`) = 繋ぐ cache がそもそも無い。この本文を
            // 55 分ごとに送り直しても延びるものは無いので、系列を畳む
            // (DR-0027 決定 8)。約束した寿命は果たされないので取り消す。
            Outcome::Sent(evidence)
                if !evidence.cache.on_cache() && evidence.cache != events::Cache::Unknown =>
            {
                debug!(
                    session = %series.session_id,
                    prefix = %series.prefix,
                    cache = evidence.cache.as_str(),
                    "the replay did not land on any cache; dropping the kept conversation"
                );
                self.expired(&series, kept.cache_notice.as_deref());
                self.forget(&series);
                // 畳んだ控えは、本文のまま研究用へ移す (DR-0027 決定 9)。
                // 捨ててしまうと、乗らなかった理由を後から見る材料が残らない。
                self.quarantine.put(&Uncached {
                    dropped: Dropped::Keepalive,
                    sent_at_ms,
                    evidence,
                    kept,
                });
            }
            Outcome::Sent(evidence) => {
                let cache = evidence.cache;
                let mut next = kept;
                // 書き直しになっていたら、繋いだのではなく作り直した。
                // 連鎖はここから数え直す。
                if cache == events::Cache::Written {
                    next.since_ms = sent_at_ms;
                    next.count = 0;
                } else {
                    next.count += 1;
                }
                next.fires_at_ms = sent_at_ms + refresh_ms();
                next.expires_at_ms = sent_at_ms + (LIFETIME - MARGIN).as_millis() as i64;
                let fires_at_ms = next.fires_at_ms;
                if !self.store.save(&next) {
                    self.forget(&series);
                    return;
                }
                self.plan(series, REFRESH_AFTER, fires_at_ms);
            }
        }
    }

    /// 送れなかった系列を、次の予定へ回す。
    ///
    /// 塞がりは解けるものなので控えは畳まない。控えの発火時刻も進めておく —
    /// 進めずにいると、読み戻した側が「とうに過ぎた予定」として即座に送りに
    /// 行く (そのときも塞がっていれば、同じことを繰り返すだけ)。
    fn postpone(self: &Arc<Self>, series: Series, mut kept: Kept, expected_ms: i64) {
        kept.fires_at_ms = expected_ms + refresh_ms();
        let fires_at_ms = kept.fires_at_ms;
        if !self.store.save(&kept) {
            self.forget(&series);
            return;
        }
        self.plan(series, REFRESH_AFTER, fires_at_ms);
    }

    /// 控えずに終わった 1 本の約束を取り消す (DR-0027 決定 8)。
    ///
    /// 送る前に「この先どこまで繋ぐ」と知らせてある ([`Chain::promised`]) ので、
    /// 控えないだけで黙ると、見る側はその見立てを描き続ける。
    pub fn not_landed(&self, sent: Sent, evidence: Evidence) {
        self.expired(&sent.series, sent.cache_notice.as_deref());
        let sent_at_ms = sent.sent_at_ms;
        self.quarantine.put(&Uncached {
            dropped: Dropped::Entry,
            sent_at_ms,
            evidence,
            kept: sent.into_kept(),
        });
    }

    /// 約束した寿命が果たされずに終わったことを知らせる (DR-0012)。
    fn expired(&self, series: &Series, promised: Option<&str>) {
        let Some(of) = promised else {
            return;
        };
        debug!(
            session = %series.session_id,
            prefix = %series.prefix,
            of,
            "the promised cache is gone; withdrawing the notice"
        );
        self.events.publish(events::CacheExpired::new(
            now_unix_ms(),
            &series.session_id,
            &series.prefix,
            of,
        ));
    }

    /// 予定を持っている系列の数。
    #[cfg(test)]
    fn armed(&self) -> usize {
        self.timers.lock().unwrap().len()
    }
}

/// 送り直す本文。控えたものの `max_tokens` だけを 1 にする (DR-0027 決定 1)。
///
/// 他は何も触らない。`stream` も `thinking` も控えたままで、`max_tokens` は
/// cache key に入らないのでプレフィックスは一致し続ける。控えに `max_tokens`
/// が無い本文 (Responses 形式) では足さない — 知らない方言へ Messages の欄を
/// 差し込むと、受け付けられるかどうかがこちらの推測になる。
pub fn body_to_send(kept: &Kept) -> Value {
    let mut body = kept.body.clone();
    if let Some(object) = body.as_object_mut()
        && object.contains_key(MAX_TOKENS_FIELD)
    {
        object.insert(MAX_TOKENS_FIELD.to_owned(), Value::from(1));
    }
    body
}

/// 間隔を、控えと同じ細かさ (ミリ秒) で。
fn refresh_ms() -> i64 {
    REFRESH_AFTER.as_millis() as i64
}

/// [`Weak`] の初期値を作るためだけの型。誰も実装を呼ばない。
struct NoSender;

impl Sender for NoSender {
    fn send<'a>(&'a self, _kept: &'a Kept) -> BoxFuture<'a, Outcome> {
        Box::pin(async { Outcome::Unsent })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 送られた本文を控える偽の upstream。
    struct FakeUpstream {
        seen: Mutex<Vec<Value>>,
        /// 何本目がいつ送られたか (壁時計の Unix ミリ秒)。
        ///
        /// 「起点が再送の瞬間へ移った」を確かめるには、その瞬間そのものが
        /// 要る。試験の側で時計を読み直すと、読んだ時刻と実装が読んだ時刻の
        /// どちらが先かは実行速度次第になる。
        at_ms: Mutex<Vec<i64>>,
        answer: Mutex<Outcome>,
        /// ここまでに送った本数。待つ側はこれが増えるのを待つ。
        count: tokio::sync::watch::Sender<usize>,
    }

    impl FakeUpstream {
        fn new(answer: Outcome) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                at_ms: Mutex::new(Vec::new()),
                answer: Mutex::new(answer),
                count: tokio::sync::watch::channel(0).0,
            })
        }

        fn sent(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }

        /// `nth` 本目が出るまで待って、それが出た時刻を返す。
        ///
        /// 待ち方は数えるのではなく知らせで ([`tokio::sync::watch`])。予定の
        /// task が走る前に読みに行くと、「起点が動いていない」と「まだ送って
        /// いない」が同じ姿になって見分けが付かない。
        ///
        /// 出ないまま止まったら待ち続けずに落とす。時計は止めてあるので、この
        /// 上限は実時間を使わずに効く (何も走っていなければ tokio が自分で
        /// 時計を進める)。
        async fn until_sent(&self, nth: usize) -> i64 {
            let mut watching = self.count.subscribe();
            tokio::time::timeout(Duration::from_secs(60), async {
                watching
                    .wait_for(|count| *count >= nth)
                    .await
                    .expect("the upstream is still there");
            })
            .await
            .unwrap_or_else(|_| panic!("replay #{nth} never went out"));
            self.at_ms.lock().unwrap()[nth - 1]
        }
    }

    impl Sender for FakeUpstream {
        fn send<'a>(&'a self, kept: &'a Kept) -> BoxFuture<'a, Outcome> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(body_to_send(kept));
                self.at_ms.lock().unwrap().push(now_unix_ms());
                self.count.send_modify(|count| *count += 1);
                self.answer.lock().unwrap().clone()
            })
        }
    }

    /// 取り消しの知らせが出るまで待って、それを返す。
    ///
    /// 時計は止めてあるので、上限は実時間を使わずに効く。
    async fn until_withdrawn(
        watching: &mut tokio::sync::broadcast::Receiver<events::Notice>,
    ) -> events::CacheExpired {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match watching.recv().await.expect("the event bus is still there") {
                    events::Notice::CacheExpired(expired) => return expired,
                    _ => continue,
                }
            }
        })
        .await
        .expect("a withdrawal was published")
    }

    /// いつでも通る経路。
    struct Open(bool);

    impl Reachable for Open {
        fn usable<'a>(&'a self, _bound: &'a Bound) -> BoxFuture<'a, bool> {
            let open = self.0;
            Box::pin(async move { open })
        }
    }

    /// 送れた 1 本の答え。cache の語以外は、普通に返った応答の値。
    fn answered(cache: events::Cache) -> Outcome {
        Outcome::Sent(Evidence {
            cache,
            status: 200,
            usage: None,
        })
    }

    /// 試験で使う「ひと昔前」。
    ///
    /// 止めた時計の下では実時計が進まないので、時刻が動いたかどうかは実行速度
    /// 次第の数ミリ秒でしか出ない。起点をこれだけ過去へ置くと、動いた / 動か
    /// ないの差が実時間と無関係に付く。
    const LONG_AGO: Duration = Duration::from_secs(10 * 60);

    fn series() -> Series {
        Series {
            session_id: "s-1".to_owned(),
            prefix: "2cf24dba".to_owned(),
        }
    }

    /// 試験では退避を効かせておく (既定と同じ)。
    fn limits() -> uncached::Limits {
        uncached::Limits { keep: 50, days: 7 }
    }

    /// 研究用に取ってある 1 本を読み戻す。無ければ `None`。
    fn set_aside(dir: &std::path::Path) -> Option<Uncached> {
        let entries = std::fs::read_dir(dir.join("keepalive").join("uncached")).ok()?;
        let path = entries.flatten().next()?.path();
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    fn body() -> Value {
        serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 32000,
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "system": [{"type": "text", "text": "you are here", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "tools": [{"name": "read"}],
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    fn sent(now_ms: i64) -> Sent {
        Sent {
            series: series(),
            ns: "default".to_owned(),
            model: "claude-opus-5".to_owned(),
            route: "a".to_owned(),
            body: body(),
            headers: vec![("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned())],
            path: "/v1/messages".to_owned(),
            query: None,
            shape: RequestShape::Messages,
            sent_at_ms: now_ms,
            horizon: Duration::from_secs(9 * 60 * 60),
            cache_notice: Some("promise-1".to_owned()),
        }
    }

    fn keepalive(
        dir: &std::path::Path,
        upstream: &Arc<FakeUpstream>,
        open: bool,
    ) -> Arc<Keepalive> {
        let keepalive = Arc::new(Keepalive::new(
            dir,
            Arc::new(Events::new()),
            Arc::new(Open(open)),
            limits(),
        ));
        keepalive.served_by(Arc::downgrade(upstream) as Weak<dyn Sender>);
        keepalive
    }

    /// 55 分後に、控えた本文が `max_tokens` 以外そのまま出ていく。
    #[tokio::test(start_paused = true)]
    async fn what_was_kept_goes_out_again_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        keepalive.armed_by_request(sent(now_unix_ms()));
        assert!(upstream.sent().is_empty(), "nothing goes out right away");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        upstream.until_sent(1).await;

        let sent = upstream.sent();
        assert_eq!(sent.len(), 1, "one replay went out");
        let mut expected = body();
        expected["max_tokens"] = Value::from(1);
        assert_eq!(sent[0], expected, "only max_tokens differs");
    }

    /// 実リクエストが来るたび、予定は先送りされる。
    #[tokio::test(start_paused = true)]
    async fn a_live_conversation_never_fires() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        for _ in 0..3 {
            keepalive.armed_by_request(sent(now_unix_ms()));
            tokio::time::advance(REFRESH_AFTER - Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        assert!(upstream.sent().is_empty(), "the replay never came due");
    }

    /// 繋いだ 1 本は、次の 55 分へまた繋がる。起点は動かさない。
    ///
    /// 起点を**意図的に過去へ**置いて始める。繋がった 1 本は連鎖を数え直さない
    /// ので、起点はその古い時刻のまま残るはず — 実時間が進んだかどうかに
    /// 関係なく、ぴたり一致で確かめられる ([`a_rewrite_starts_the_chain_over`]
    /// が確かめる「動く」側の対。両方を実時計の進みに頼らず書く)。
    #[tokio::test(start_paused = true)]
    async fn a_hit_leads_to_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        let started = now_unix_ms() - LONG_AGO.as_millis() as i64;
        keepalive.armed_by_request(sent(started));
        for nth in 1..=3 {
            tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
            upstream.until_sent(nth).await;
        }
        assert_eq!(upstream.sent().len(), 3, "it kept going");
        let kept = keepalive.store.load(&series()).unwrap();
        assert_eq!(kept.count, 3, "each replay is counted");
        assert_eq!(
            kept.since_ms, started,
            "and the origin stays at the last real request"
        );
    }

    /// 書き直しになったら、連鎖はそこから数え直す。
    ///
    /// 起点を**意図的に過去へ**置いて始める。時計は止めてあるので、実時計は
    /// 55 分進めても動かない (測って 0 ミリ秒) — 「今」を起点にすると、起点が
    /// 動いたかどうかが試験の実行速度に懸かる。古い起点から始めれば、動いた
    /// 側は [`LONG_AGO`] ぶん離れる。
    #[tokio::test(start_paused = true)]
    async fn a_rewrite_starts_the_chain_over() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Written));
        let keepalive = keepalive(dir.path(), &upstream, true);

        let started = now_unix_ms() - LONG_AGO.as_millis() as i64;
        keepalive.armed_by_request(sent(started));
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let rewritten_at = upstream.until_sent(1).await;

        let kept = keepalive.store.load(&series()).unwrap();
        assert_eq!(kept.count, 0, "the chain is counted from the rewrite");
        assert!(
            kept.since_ms >= rewritten_at,
            "the origin moved to the moment of the rewrite"
        );
        assert!(
            kept.since_ms > started,
            "so it no longer sits at the request that was replaced"
        );
    }

    /// 乗るものが無かった系列は、そこで畳む (DR-0027 決定 8)。
    ///
    /// 送れてはいるので塞がり ([`Outcome::Unsent`]) とは別物。繋ぐ cache が
    /// 無いまま 55 分ごとに全量入力を払い続けることになるので、控えを捨てて
    /// 約束を取り消す。会話が戻ってくれば、実リクエスト 1 本がまた控えを置く。
    #[tokio::test(start_paused = true)]
    async fn a_replay_that_lands_on_nothing_ends_the_series() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::None));
        let events = Arc::new(Events::new());
        let keepalive = Arc::new(Keepalive::new(
            dir.path(),
            Arc::clone(&events),
            Arc::new(Open(true)),
            limits(),
        ));
        keepalive.served_by(Arc::downgrade(&upstream) as Weak<dyn Sender>);
        let mut watching = events.subscribe();

        keepalive.armed_by_request(sent(now_unix_ms()));
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        upstream.until_sent(1).await;

        assert_eq!(upstream.sent().len(), 1, "one replay went out");
        assert_eq!(
            keepalive.store.load(&series()),
            None,
            "the series was dropped instead of being replayed again"
        );
        assert_eq!(keepalive.armed(), 0, "nothing is watched any more");
        match watching.try_recv().expect("a withdrawal was published") {
            events::Notice::CacheExpired(expired) => {
                assert_eq!(expired.of, "promise-1", "it names the promise it withdrew");
            }
            other => panic!("expected a withdrawal, got {}", other.name()),
        }

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(upstream.sent().len(), 1, "and it never went out again");
    }

    /// 畳んだ系列の本文は、研究用に取ってある (DR-0027 決定 9)。
    ///
    /// 畳むのは「繋ぐ cache が無い」と分かったからだが、**なぜ無いのか**は
    /// そこでは分からない。本文と判定の材料を残しておかないと、後から見る
    /// ものが何も無くなる。
    #[tokio::test(start_paused = true)]
    async fn a_replay_that_lands_on_nothing_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(Evidence {
            cache: events::Cache::None,
            status: 200,
            usage: Some(crate::metering::TokenUsage::default()),
        }));
        let keepalive = keepalive(dir.path(), &upstream, true);

        keepalive.armed_by_request(sent(now_unix_ms()));
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        upstream.until_sent(1).await;

        let aside = set_aside(dir.path()).expect("the replay was set aside");
        assert_eq!(aside.dropped, Dropped::Keepalive, "it was the self-send");
        assert_eq!(aside.evidence.cache, events::Cache::None);
        assert_eq!(aside.evidence.status, 200);
        assert_eq!(
            aside.evidence.usage,
            Some(crate::metering::TokenUsage::default())
        );
        assert_eq!(aside.kept.body, body(), "the body itself is there");
        assert_eq!(aside.kept.session_id, series().session_id);
        assert_eq!(aside.kept.prefix, series().prefix);
        assert_eq!(aside.kept.ns, "default");
        assert_eq!(aside.kept.model, "claude-opus-5");
        assert_eq!(aside.kept.route, "a");
    }

    /// 入口で控えなかった 1 本も、同じところに取ってある。
    #[tokio::test(start_paused = true)]
    async fn a_request_that_did_not_land_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        let started = now_unix_ms();
        keepalive.not_landed(
            sent(started),
            Evidence {
                cache: events::Cache::None,
                status: 200,
                usage: None,
            },
        );

        assert_eq!(
            keepalive.store.load(&series()),
            None,
            "it is not kept for replay"
        );
        let aside = set_aside(dir.path()).expect("the request was set aside");
        assert_eq!(aside.dropped, Dropped::Entry, "it was the real request");
        assert_eq!(aside.sent_at_ms, started);
        assert_eq!(aside.evidence.cache, events::Cache::None);
        assert_eq!(aside.kept.body, body(), "the body itself is there");
    }

    /// 上限が 0 なら、乗らなかった 1 本も取っておかない。
    #[tokio::test(start_paused = true)]
    async fn nothing_is_set_aside_when_it_is_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = Arc::new(Keepalive::new(
            dir.path(),
            Arc::new(Events::new()),
            Arc::new(Open(true)),
            uncached::Limits { keep: 0, days: 7 },
        ));
        keepalive.served_by(Arc::downgrade(&upstream) as Weak<dyn Sender>);

        keepalive.not_landed(
            sent(now_unix_ms()),
            Evidence {
                cache: events::Cache::None,
                status: 200,
                usage: None,
            },
        );

        assert!(set_aside(dir.path()).is_none(), "nothing was written");
    }

    /// usage が読めなかった 1 本では畳まない。
    ///
    /// 切れた応答・usage を載せない口がこれ。cache が無いと分かったわけでは
    /// ないので、塞がりと同じく次の予定へ回す。
    #[tokio::test(start_paused = true)]
    async fn a_replay_whose_usage_was_unreadable_keeps_the_series() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Unknown));
        let keepalive = keepalive(dir.path(), &upstream, true);

        keepalive.armed_by_request(sent(now_unix_ms()));
        for nth in 1..=2 {
            tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
            upstream.until_sent(nth).await;
        }
        assert_eq!(upstream.sent().len(), 2, "it kept going");
        assert!(
            keepalive.store.load(&series()).is_some(),
            "the series is kept"
        );
    }

    /// 経路が塞がっていたら送らず、次の予定へ回す。
    #[tokio::test(start_paused = true)]
    async fn a_blocked_route_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, false);

        keepalive.armed_by_request(sent(now_unix_ms()));
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(upstream.sent().is_empty(), "nothing went out");
        assert_eq!(keepalive.armed(), 1, "the series is still watched");
    }

    /// 期限の切れた系列は、送らずに畳んで取り消しを出す。
    #[tokio::test(start_paused = true)]
    async fn an_expired_series_is_withdrawn_instead_of_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let events = Arc::new(Events::new());
        let keepalive = Arc::new(Keepalive::new(
            dir.path(),
            Arc::clone(&events),
            Arc::new(Open(true)),
            limits(),
        ));
        keepalive.served_by(Arc::downgrade(&upstream) as Weak<dyn Sender>);
        let mut watching = events.subscribe();

        // 予定より先に期限が来ている控え = 機械が眠っていた後の姿。
        let mut kept = {
            keepalive.armed_by_request(sent(now_unix_ms()));
            keepalive.store.load(&series()).unwrap()
        };
        kept.expires_at_ms = now_unix_ms() - 1;
        keepalive.store.save(&kept);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        // 何も送らずに畳む場面なので、待つ相手は送信ではなく取り消しの知らせ。
        let expired = until_withdrawn(&mut watching).await;

        assert!(
            upstream.sent().is_empty(),
            "there was nothing left to extend"
        );
        assert_eq!(
            keepalive.store.load(&series()),
            None,
            "the series was dropped"
        );
        assert_eq!(expired.kind, events::CacheExpired::KIND);
        assert_eq!(expired.of, "promise-1", "it names the promise it withdrew");
        assert_eq!(expired.prefix, series().prefix);
    }

    /// 期間が尽きた系列は黙って畳む (最後の 1 本が置いた cache はまだ生きている)。
    #[tokio::test(start_paused = true)]
    async fn a_finished_horizon_ends_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        keepalive.armed_by_request(sent(now_unix_ms()));
        // 期間だけが尽きた控え = 予定より先に horizon が来ていた姿。
        let mut kept = keepalive.store.load(&series()).unwrap();
        kept.horizon_end_ms = now_unix_ms() - 1;
        keepalive.store.save(&kept);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(upstream.sent().is_empty(), "the horizon was over");
        assert_eq!(
            keepalive.store.load(&series()),
            None,
            "the series was dropped"
        );
    }

    /// 兄弟が掴んでいる系列は撫でない。
    #[tokio::test(start_paused = true)]
    async fn a_series_a_sibling_holds_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);
        keepalive.armed_by_request(sent(now_unix_ms()));

        // 兄弟のつもりで掴んでおく。
        let sibling = store::Store::new(dir.path());
        let _held = sibling.claim(&series()).expect("the sibling took it");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(upstream.sent().is_empty(), "the sibling is doing it");
        assert_eq!(keepalive.armed(), 1, "the series is still watched");
    }

    /// 大きすぎる会話は控えず、予定も置かない。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_too_large_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        let mut huge = sent(now_unix_ms());
        huge.body = serde_json::json!({ "text": "x".repeat(store::BODY_LIMIT + 1) });
        keepalive.armed_by_request(huge);

        assert_eq!(keepalive.armed(), 0, "nothing is watched");
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(upstream.sent().is_empty());
    }

    /// 止めた会話の控えは消える。解くのは実リクエスト 1 本。
    #[tokio::test(start_paused = true)]
    async fn pausing_drops_the_kept_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);
        keepalive.armed_by_request(sent(now_unix_ms()));

        keepalive.pause("s-1");
        assert_eq!(keepalive.store.load(&series()), None);
        assert_eq!(keepalive.armed(), 0);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            upstream.sent().is_empty(),
            "a paused conversation stays quiet"
        );

        // 実リクエストが 1 本来れば、そのまま張り直る。
        keepalive.armed_by_request(sent(now_unix_ms()));
        assert_eq!(keepalive.armed(), 1);
    }

    /// 落ちている間の控えを読み戻して、予定を張り直す。
    #[tokio::test(start_paused = true)]
    async fn what_was_kept_is_picked_up_again() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        {
            let before = keepalive(dir.path(), &upstream, true);
            before.armed_by_request(sent(now_unix_ms()));
        }

        let after = keepalive(dir.path(), &upstream, true);
        after.restore();
        assert_eq!(after.armed(), 1, "the kept series came back");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        upstream.until_sent(1).await;
        assert_eq!(upstream.sent().len(), 1, "it replayed on the old schedule");
    }

    /// 読み戻しのついでに、古びた退避も切り詰める。
    ///
    /// 落ちている間は誰も書かないので、上限を超えたまま残る分はここでしか
    /// 減らない。
    #[tokio::test(start_paused = true)]
    async fn picking_up_also_trims_what_was_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let stale = dir.path().join("keepalive").join("uncached");
        std::fs::create_dir_all(&stale).unwrap();
        // 期間の外に置いてある 1 本 = 前回動いていた頃の退避。
        let long_ago = now_unix_ms() - 30 * 24 * 60 * 60 * 1000;
        let path = stale.join(format!("s-1.2cf24dba.{long_ago}.json"));
        std::fs::write(&path, b"{}").unwrap();

        keepalive(dir.path(), &upstream, true).restore();

        assert!(!path.exists(), "the stale one was dropped on the way up");
    }

    /// 連鎖の見立ては、起点から 55 分刻みで並ぶ。
    #[test]
    fn the_chain_is_laid_out_from_the_origin() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::new(dir.path());
        let started = 1_800_000_000_000;
        store.save(&Kept {
            session_id: "s-1".to_owned(),
            prefix: "2cf24dba".to_owned(),
            ns: "default".to_owned(),
            model: "m".to_owned(),
            route: "a".to_owned(),
            body: body(),
            headers: Vec::new(),
            path: "/v1/messages".to_owned(),
            query: None,
            shape: RequestShape::Messages,
            fires_at_ms: started + refresh_ms(),
            expires_at_ms: started + (LIFETIME - MARGIN).as_millis() as i64,
            horizon_end_ms: started + 2 * refresh_ms(),
            since_ms: started,
            count: 0,
            cache_notice: None,
        });
        let keepalive = Keepalive::new(
            dir.path(),
            Arc::new(Events::new()),
            Arc::new(Open(true)),
            limits(),
        );

        let chain = keepalive.chain(&series()).unwrap();
        assert_eq!(chain.since_ms, started);
        assert_eq!(chain.count, 0);
        assert_eq!(chain.next_at_ms, Some(started + refresh_ms()));
        // 期間の終わり (2 本目の予定と同時刻) を跨いだ 1 本まで出る。
        assert_eq!(chain.until_count, 2);
        assert_eq!(
            chain.until_ms,
            started + 2 * refresh_ms() + LIFETIME.as_millis() as i64
        );
    }

    /// 送る前に出す見立てと、控えを置いた後に読める連鎖は同じもの。
    ///
    /// 知らせに出すのは前者 (1 本目から出せる)、見張りが使うのは後者。両者が
    /// ずれると、見る側は「言われた終わり」と「実際に繋ぐ終わり」の違う 2 つを
    /// 相手にすることになる。
    #[tokio::test(start_paused = true)]
    async fn what_was_promised_is_what_gets_kept() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(answered(events::Cache::Hit));
        let keepalive = keepalive(dir.path(), &upstream, true);

        let started = now_unix_ms();
        let sent = sent(started);
        let horizon = sent.horizon;
        keepalive.armed_by_request(sent);

        assert_eq!(
            keepalive.chain(&series()),
            Some(Chain::promised(started, horizon)),
            "the notice promised exactly what the kept series reports"
        );
    }

    /// 分岐点の本数も、実際の連鎖と同じ数え方で並ぶ。
    #[test]
    fn the_break_even_is_counted_the_same_way() {
        let since_ms = 1_800_000_000_000;
        for (minutes, count) in [(0, 1), (30, 1), (55, 1), (110, 2), (120, 3)] {
            let breakeven = Breakeven::from(since_ms, Duration::from_secs(minutes * 60));
            assert_eq!(breakeven.count, count, "over {minutes} minutes");
            assert_eq!(
                breakeven.until_ms,
                since_ms + (REFRESH_AFTER * count + LIFETIME).as_millis() as i64
            );
        }
    }

    /// 道具を持たない 1 本は会話の本流ではない。
    #[test]
    fn a_request_without_tools_is_not_the_conversation() {
        assert!(carries_tools(
            &serde_json::json!({"tools": [{"name": "Bash"}]})
        ));
        for body in [
            serde_json::json!({}),
            serde_json::json!({"tools": []}),
            serde_json::json!({"tools": "none"}),
        ] {
            assert!(!carries_tools(&body), "{body}");
        }
    }

    /// 約束の id は当てられない。JSON にそのまま乗る文字だけで出来ている。
    #[test]
    fn each_notice_id_is_unpredictable_and_url_safe() {
        let ids: std::collections::HashSet<String> = (0..64).map(|_| notice_id()).collect();
        assert_eq!(ids.len(), 64, "no two promises share a name");
        for id in &ids {
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "the id travels in JSON as it is: {id}"
            );
        }
    }
}
