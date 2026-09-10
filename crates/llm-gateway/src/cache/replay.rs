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

use crate::cache::keepalive::{self, Bound, Chain, Reachable};
use crate::credential::time::now_unix_ms;
use crate::egress::{BoxFuture, RequestShape};
use crate::events::{self, Events};

pub mod store;

pub use store::{Kept, Series};

/// 1 本送ってから、次に送り直すまでの時間。
///
/// cache が消える手前。会話が動いている間は実リクエストのたびに先送りされる
/// ので、ここまで空くこと自体が「止まった」の合図になる。
const REFRESH_AFTER: Duration = Duration::from_secs(55 * 60);

/// 送った本文が残す cache の寿命 (`replay` は全ブレークポイントが 1 時間)。
const LIFETIME: Duration = Duration::from_secs(60 * 60);

/// 期限にどれだけ余裕を見るか。
///
/// 送ってから upstream が前処理を始めるまでの分。切り詰めると、間に合った
/// つもりの 1 本が全量の書き直しになる。
const MARGIN: Duration = Duration::from_secs(30);

/// 送り直しで 1 にする欄。ここだけが元の本文と変わる。
const MAX_TOKENS_FIELD: &str = "max_tokens";

/// 控えた 1 本を実際に送る役。
///
/// 経路を選び直して認証を付けるのは転送側の仕事なので、こちらは頼むだけ
/// (DR-0014 §3 と同じ切り方)。
pub trait Sender: Send + Sync {
    /// この本文を upstream へ送り、cache がどうなったかを返す。
    fn replay<'a>(&'a self, kept: &'a Kept) -> BoxFuture<'a, Outcome>;
}

/// 送り直した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 送れた。usage から読んだ cache の実結果が入る。
    Sent(events::Cache),
    /// 送れなかった (経路が塞がっていた・転送に失敗した)。次の予定へ回す。
    Unsent,
}

/// 送り直しの仕掛け。
pub struct Replay {
    store: store::Store,
    events: Arc<Events>,
    /// 直前に通った経路がまだ使えるかを聞く先。
    reach: Arc<dyn Reachable>,
    /// 実際に送る役。転送側が自分を渡す。
    ///
    /// 弱い参照で持つのは、転送側が[`Replay`]を持つため — 強い参照だと輪に
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

impl Replay {
    pub fn new(
        dir: impl AsRef<std::path::Path>,
        events: Arc<Events>,
        reach: Arc<dyn Reachable>,
    ) -> Self {
        Self {
            store: store::Store::new(dir),
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
        let kept = Kept {
            session_id: sent.series.session_id.clone(),
            prefix: sent.series.prefix.clone(),
            ns: sent.ns,
            model: sent.model,
            route: sent.route,
            body: sent.body,
            headers: sent.headers,
            path: sent.path,
            query: sent.query,
            shape: sent.shape,
            fires_at_ms: sent.sent_at_ms + refresh_ms(),
            expires_at_ms: sent.sent_at_ms + (LIFETIME - MARGIN).as_millis() as i64,
            horizon_end_ms: sent.sent_at_ms + sent.horizon.as_millis() as i64,
            since_ms: sent.sent_at_ms,
            count: 0,
            cache_notice: sent.cache_notice,
        };
        if !self.store.save(&kept) {
            // 置けなかった系列は繋げない。前の予定まで残すと、控えの無い
            // 系列を撫でに行くだけになる。
            self.forget(&sent.series);
            return;
        }
        self.plan(sent.series, REFRESH_AFTER, kept.fires_at_ms);
    }

    /// この系列の控えと予定を捨てる。
    ///
    /// 呼ぶのは、繋ぐ相手が無くなったとき (期限切れ・期間の終わり) と、
    /// 人が止めたとき ([`Self::pause`])。
    pub fn forget(&self, series: &Series) {
        self.timers.lock().unwrap().remove(series);
        self.store.remove(series);
    }

    /// この会話への送り直しを止める (DR-0024 §2 追補の pause API)。
    ///
    /// 会話の id を持つ**全系列**の控えを落とす。解除は実リクエスト 1 本で
    /// 自動 — 控えが無い系列へ来た 1 本は、そのまま新しい控えを置く
    /// ([`Self::armed_by_request`])。止めたことを覚えておく必要は無い
    /// (合図方式と違い、兄弟へ渡す停止の一覧も要らない。置き場が 1 つなので、
    /// 落ちた控えは兄弟からも消えている)。
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
        keepalive::notice_id()
    }

    /// この系列に立っている送り直しの連鎖 (DR-0012 の `cache_*`)。
    ///
    /// **見込み値**。経路が塞がって送れなければ、ここより早く切れる。
    pub fn chain(&self, series: &Series) -> Option<Chain> {
        let kept = self.store.load(series)?;
        let at = |signal: u32| kept.since_ms + i64::from(signal) * refresh_ms();
        // 期間の終わりを跨いだ 1 本まで出る (発火時点で期間が残っていれば
        // 次を仕込む)。次の 1 本の後に、あと何本続くか。
        let more = ((kept.horizon_end_ms - kept.fires_at_ms).max(0) as u64)
            .div_ceil(refresh_ms() as u64) as u32;
        let until_count = kept.count + 1 + more;
        Some(Chain {
            since_ms: kept.since_ms,
            count: kept.count,
            next_at_ms: Some(at(kept.count + 1)),
            until_ms: at(until_count) + LIFETIME.as_millis() as i64,
            until_count,
        })
    }

    /// この系列の実リクエストが最後に通った先。
    pub fn bound(&self, series: &Series) -> Option<Bound> {
        let kept = self.store.load(series)?;
        Some(Bound {
            ns: kept.ns,
            model: kept.model,
            route: kept.route,
        })
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

        let outcome = sender.replay(&kept).await;
        let sent_at_ms = now_unix_ms();
        match outcome {
            Outcome::Unsent => {
                self.postpone(series, kept, expected_ms);
                return;
            }
            Outcome::Sent(cache) => {
                let mut next = kept;
                // 書き直しになっていたら、繋いだのではなく作り直した
                // (DR-0024 §2 追補と同じ扱い)。連鎖はここから数え直す。
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
    fn replay<'a>(&'a self, _kept: &'a Kept) -> BoxFuture<'a, Outcome> {
        Box::pin(async { Outcome::Unsent })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 送られた本文を控える偽の upstream。
    struct FakeUpstream {
        seen: Mutex<Vec<Value>>,
        answer: Mutex<Outcome>,
    }

    impl FakeUpstream {
        fn new(answer: Outcome) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                answer: Mutex::new(answer),
            })
        }

        fn sent(&self) -> Vec<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Sender for FakeUpstream {
        fn replay<'a>(&'a self, kept: &'a Kept) -> BoxFuture<'a, Outcome> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(body_to_send(kept));
                *self.answer.lock().unwrap()
            })
        }
    }

    /// いつでも通る経路。
    struct Open(bool);

    impl Reachable for Open {
        fn usable<'a>(&'a self, _bound: &'a Bound) -> BoxFuture<'a, bool> {
            let open = self.0;
            Box::pin(async move { open })
        }
    }

    fn series() -> Series {
        Series {
            session_id: "s-1".to_owned(),
            prefix: "2cf24dba".to_owned(),
        }
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

    fn replay(dir: &std::path::Path, upstream: &Arc<FakeUpstream>, open: bool) -> Arc<Replay> {
        let replay = Arc::new(Replay::new(
            dir,
            Arc::new(Events::new()),
            Arc::new(Open(open)),
        ));
        replay.served_by(Arc::downgrade(upstream) as Weak<dyn Sender>);
        replay
    }

    /// 55 分後に、控えた本文が `max_tokens` 以外そのまま出ていく。
    #[tokio::test(start_paused = true)]
    async fn what_was_kept_goes_out_again_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);

        replay.armed_by_request(sent(now_unix_ms()));
        assert!(upstream.sent().is_empty(), "nothing goes out right away");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

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
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);

        for _ in 0..3 {
            replay.armed_by_request(sent(now_unix_ms()));
            tokio::time::advance(REFRESH_AFTER - Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        assert!(upstream.sent().is_empty(), "the replay never came due");
    }

    /// 繋いだ 1 本は、次の 55 分へまた繋がる。
    #[tokio::test(start_paused = true)]
    async fn a_hit_leads_to_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);

        replay.armed_by_request(sent(now_unix_ms()));
        for _ in 0..3 {
            tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(upstream.sent().len(), 3, "it kept going");
        let kept = replay.store.load(&series()).unwrap();
        assert_eq!(kept.count, 3, "each replay is counted");
    }

    /// 書き直しになったら、連鎖はそこから数え直す。
    #[tokio::test(start_paused = true)]
    async fn a_rewrite_starts_the_chain_over() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Written));
        let replay = replay(dir.path(), &upstream, true);

        let started = now_unix_ms();
        replay.armed_by_request(sent(started));
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        let kept = replay.store.load(&series()).unwrap();
        assert_eq!(kept.count, 0, "the chain is counted from the rewrite");
        assert!(kept.since_ms > started, "the origin moved to the rewrite");
    }

    /// 経路が塞がっていたら送らず、次の予定へ回す。
    #[tokio::test(start_paused = true)]
    async fn a_blocked_route_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, false);

        replay.armed_by_request(sent(now_unix_ms()));
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(upstream.sent().is_empty(), "nothing went out");
        assert_eq!(replay.armed(), 1, "the series is still watched");
    }

    /// 期限の切れた系列は、送らずに畳んで取り消しを出す。
    #[tokio::test(start_paused = true)]
    async fn an_expired_series_is_withdrawn_instead_of_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let events = Arc::new(Events::new());
        let replay = Arc::new(Replay::new(
            dir.path(),
            Arc::clone(&events),
            Arc::new(Open(true)),
        ));
        replay.served_by(Arc::downgrade(&upstream) as Weak<dyn Sender>);
        let mut watching = events.subscribe();

        // 予定より先に期限が来ている控え = 機械が眠っていた後の姿。
        let mut kept = {
            replay.armed_by_request(sent(now_unix_ms()));
            replay.store.load(&series()).unwrap()
        };
        kept.expires_at_ms = now_unix_ms() - 1;
        replay.store.save(&kept);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(
            upstream.sent().is_empty(),
            "there was nothing left to extend"
        );
        assert_eq!(replay.store.load(&series()), None, "the series was dropped");
        match watching.try_recv().expect("a withdrawal was published") {
            events::Notice::CacheExpired(expired) => {
                assert_eq!(expired.kind, events::CacheExpired::KIND);
                assert_eq!(expired.of, "promise-1", "it names the promise it withdrew");
                assert_eq!(expired.prefix, series().prefix);
            }
            other => panic!("expected a withdrawal, got {}", other.name()),
        }
    }

    /// 期間が尽きた系列は黙って畳む (最後の 1 本が置いた cache はまだ生きている)。
    #[tokio::test(start_paused = true)]
    async fn a_finished_horizon_ends_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);

        replay.armed_by_request(sent(now_unix_ms()));
        // 期間だけが尽きた控え = 予定より先に horizon が来ていた姿。
        let mut kept = replay.store.load(&series()).unwrap();
        kept.horizon_end_ms = now_unix_ms() - 1;
        replay.store.save(&kept);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(upstream.sent().is_empty(), "the horizon was over");
        assert_eq!(replay.store.load(&series()), None, "the series was dropped");
    }

    /// 兄弟が掴んでいる系列は撫でない。
    #[tokio::test(start_paused = true)]
    async fn a_series_a_sibling_holds_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);
        replay.armed_by_request(sent(now_unix_ms()));

        // 兄弟のつもりで掴んでおく。
        let sibling = store::Store::new(dir.path());
        let _held = sibling.claim(&series()).expect("the sibling took it");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        assert!(upstream.sent().is_empty(), "the sibling is doing it");
        assert_eq!(replay.armed(), 1, "the series is still watched");
    }

    /// 大きすぎる会話は控えず、予定も置かない。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_too_large_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);

        let mut huge = sent(now_unix_ms());
        huge.body = serde_json::json!({ "text": "x".repeat(store::BODY_LIMIT + 1) });
        replay.armed_by_request(huge);

        assert_eq!(replay.armed(), 0, "nothing is watched");
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(upstream.sent().is_empty());
    }

    /// 止めた会話の控えは消える。解くのは実リクエスト 1 本。
    #[tokio::test(start_paused = true)]
    async fn pausing_drops_the_kept_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        let replay = replay(dir.path(), &upstream, true);
        replay.armed_by_request(sent(now_unix_ms()));

        replay.pause("s-1");
        assert_eq!(replay.store.load(&series()), None);
        assert_eq!(replay.armed(), 0);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            upstream.sent().is_empty(),
            "a paused conversation stays quiet"
        );

        // 実リクエストが 1 本来れば、そのまま張り直る。
        replay.armed_by_request(sent(now_unix_ms()));
        assert_eq!(replay.armed(), 1);
    }

    /// 落ちている間の控えを読み戻して、予定を張り直す。
    #[tokio::test(start_paused = true)]
    async fn what_was_kept_is_picked_up_again() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = FakeUpstream::new(Outcome::Sent(events::Cache::Hit));
        {
            let before = replay(dir.path(), &upstream, true);
            before.armed_by_request(sent(now_unix_ms()));
        }

        let after = replay(dir.path(), &upstream, true);
        after.restore();
        assert_eq!(after.armed(), 1, "the kept series came back");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(upstream.sent().len(), 1, "it replayed on the old schedule");
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
        let replay = Replay::new(dir.path(), Arc::new(Events::new()), Arc::new(Open(true)));

        let chain = replay.chain(&series()).unwrap();
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
}
