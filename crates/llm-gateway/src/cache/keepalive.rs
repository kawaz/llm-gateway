//! 止まった会話へ、cache を延ばす合図を出す (DR-0024 §2)。
//!
//! 1 時間の cache は、会話が止まった 1 時間後に消える。消えてから再開すると、
//! その時点のプレフィックス全量を書き直すことになり、書き込みは読み出しの
//! 50 倍の単価で効く。そこで**消える手前で 1 往復だけ挟む**。その 1 本は
//! プレフィックス全量の read で済み、次の 1 時間へ繋がる。
//!
//! こちらから会話へ話し掛ける口は持っていない。合図は受け口 (DR-0012) へ
//! 流し、文面を会話へ流し込むのは受け取った側 (ccmsg) の仕事。戻ってきた
//! リクエストは合言葉 (nonce) で見分ける。
//!
//! 状態はプロセス内のメモリだけ。落ちれば消えるが、次の実リクエストで
//! 張り直されるので、失うのは合図 1 回分の機会だけ。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::debug;

use crate::credential::time::now_unix;
use crate::egress::BoxFuture;
use crate::events::{self, Events};

/// 別のプロセスが出した合図を見た後、こちらが出しに行くまでの時間。
///
/// cache が消える手前 ([`LIFETIME`] − [`MARGIN`] = 59 分 30 秒) より前に置く。
/// 相手が生きていれば、こちらが出す前に相手の次の合図が届いて、また後ろへ
/// 下がる。相手が居なくなったときだけ、こちらが引き継ぐ (DR-0024 §2)。
const STANDBY_AFTER: Duration = Duration::from_secs(57 * 60);

/// 1 本送ってから、次の合図を出すまでの時間。
///
/// cache が消える手前。会話が動いている間は次のリクエストのたびに先送りされる
/// ので、ここまで空くこと自体が「止まった」の合図になる。
const REFRESH_AFTER: Duration = Duration::from_secs(55 * 60);

/// 送った本文が残す cache の寿命 (`keepalive` は全ブレークポイントが 1 時間)。
const LIFETIME: Duration = Duration::from_secs(60 * 60);

/// 期限にどれだけ余裕を見るか。
///
/// 合図が届いてから upstream が前処理を始めるまでの分。切り詰めると、
/// 間に合ったつもりの往復が全量の書き直しになる。
const MARGIN: Duration = Duration::from_secs(30);

/// 合言葉の長さ (バイト)。
const NONCE_BYTES: usize = 32;

/// 会話系列。同じ会話でも、系列が違えば別の cache になる (DR-0012 の `prefix`)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Series {
    pub session_id: String,
    pub prefix: String,
}

/// 戻ってきた合図の扱い。
///
/// どちらでも本文の扱いは同じ (戦略が全ブレークポイントに 1 時間を付ける)。
/// 分けているのは、合図が役に立ったかを見る側に伝えるため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// 期限内に戻ってきた。狙いどおり cache が繋がる。
    Applied,
    /// 期限を過ぎていた。cache は既に消えていて、この 1 本が書き直す。
    Late,
    /// **こちらが出していない**合言葉。同じ会話を見ている別のプロセスが
    /// 出した合図で、cache はそちらが繋いでいる (DR-0024 §2)。
    Foreign,
}

impl Marker {
    /// 知らせに出す 1 語。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Late => "late",
            Self::Foreign => "foreign",
        }
    }
}

/// 直前の実リクエストが通った先。
///
/// 合図を出す前に、そこがまだ使えるかを確かめるために覚えておく。
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

/// 合図を出す仕掛け。
pub struct Keepalive {
    events: Arc<Events>,
    /// 直前に通った経路がまだ使えるかを聞く先。
    reach: Arc<dyn Reachable>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// 見張っている系列。
    watched: HashMap<Series, Watched>,
    /// 出したまま戻ってきていない合図。
    pending: HashMap<String, Pending>,
}

struct Watched {
    /// 次に合図を出す予定。出した直後は空 (戻りを待っている間)。
    timer: Option<Timer>,
    /// この系列に合図を出し続ける終わり。実リクエストのたびに先へ延びる。
    horizon_end: Instant,
    /// 直前の実リクエストが通った先。合図の往復では書き換えない。
    bound: Bound,
}

/// 予定の実体。畳まれたら止まる。
struct Timer(JoinHandle<()>);

impl Drop for Timer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Pending {
    series: Series,
    /// これを過ぎて戻ってきた合図には 1 時間を付けない。
    deadline: Instant,
}

impl Keepalive {
    pub fn new(events: Arc<Events>, reach: Arc<dyn Reachable>) -> Self {
        Self {
            events,
            reach,
            state: Mutex::new(State::default()),
        }
    }

    /// 実リクエストを送った。見張りを張り直す。
    ///
    /// 前の予定は捨てる。**次のリクエストが来るたびに先送りされる**ので、
    /// 会話が動いている間は一度も発火しない。見張る期間 (`horizon`) と
    /// 通った先を延ばせるのは、この 1 本だけ。
    pub fn armed_by_request(self: &Arc<Self>, series: Series, bound: Bound, horizon: Duration) {
        let horizon_end = Instant::now() + horizon;
        self.schedule(series, REFRESH_AFTER, horizon_end, bound);
    }

    /// 自分が出した合図が戻ってきた。同じ期間の中で次の予定だけ置き直す。
    ///
    /// 期間を延ばせるのは実リクエストだけなので、`horizon` を過ぎた系列は
    /// ここで見張るのをやめる。1 時間の cache を延々と継ぎ足す価値があるのは、
    /// 再開される見込みがある間だけ (DR-0024 §3)。
    pub fn rearm(self: &Arc<Self>, series: Series) {
        let Some((horizon_end, bound)) = self.watch_of(&series) else {
            return;
        };
        self.schedule(series, REFRESH_AFTER, horizon_end, bound);
    }

    /// 別のプロセスが出した合図を見た。一歩下がって控える (DR-0024 §2)。
    ///
    /// 相手が生きている限り、相手の合図が届くたびにここへ戻ってきて予定が
    /// 後ろへ延びる (= こちらは一度も出さない)。相手が居なくなったときだけ
    /// [`STANDBY_AFTER`] で発火して引き継ぐ。共有する状態を持たずに、
    /// 見えているものだけで 1 本へ収束する。
    ///
    /// この系列を見たことのないプロセスでは、その合図が通った先を起点にする
    /// — 実リクエストを見ていなくても、控えには入れる。
    pub fn standby(self: &Arc<Self>, series: Series, bound: Bound, horizon: Duration) {
        let (horizon_end, bound) = self
            .watch_of(&series)
            .unwrap_or_else(|| (Instant::now() + horizon, bound));
        self.schedule(series, STANDBY_AFTER, horizon_end, bound);
    }

    /// 見張っている系列の、期間の終わりと通った先。期間を過ぎていれば畳む。
    fn watch_of(&self, series: &Series) -> Option<(Instant, Bound)> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        match state.watched.get(series) {
            Some(watched) if watched.horizon_end > now => {
                Some((watched.horizon_end, watched.bound.clone()))
            }
            Some(_) => {
                state.watched.remove(series);
                None
            }
            None => None,
        }
    }

    /// 次に合図を出す時刻を置く。
    fn schedule(
        self: &Arc<Self>,
        series: Series,
        after: Duration,
        horizon_end: Instant,
        bound: Bound,
    ) {
        let now = Instant::now();
        let fires_at = now + after;
        let deadline = now + LIFETIME - MARGIN;
        let deadline_unix = now_unix() + (LIFETIME - MARGIN).as_secs() as i64;
        let waking = Arc::clone(self);
        let ringing = series.clone();
        let timer = Timer(tokio::spawn(async move {
            tokio::time::sleep_until(fires_at).await;
            waking.fire(ringing, deadline, deadline_unix).await;
        }));
        // 前の予定は差し替えで畳まれる (`Timer` の Drop が止める)。
        self.state.lock().unwrap().watched.insert(
            series,
            Watched {
                timer: Some(timer),
                horizon_end,
                bound,
            },
        );
    }

    /// この系列の実リクエストが最後に通った先。
    pub fn bound(&self, series: &Series) -> Option<Bound> {
        self.state
            .lock()
            .unwrap()
            .watched
            .get(series)
            .map(|watched| watched.bound.clone())
    }

    /// この系列の合図待ちを畳む。
    ///
    /// 合言葉を持たないリクエストが来た = 人が会話を再開した。出したままの
    /// 合図は用済みで、戻ってきても 1 時間を付ける理由がない。
    pub fn forget(&self, series: &Series) {
        let mut state = self.state.lock().unwrap();
        state.watched.remove(series);
        state.pending.retain(|_, pending| &pending.series != series);
    }

    /// 本文が合図の戻りなら、合言葉を使い切って扱いを返す。
    ///
    /// 合言葉は 1 回だけ有効。**出した覚えのない合言葉も合図の戻り** —
    /// 同じ会話を見ている別のプロセスが出したもので、2 度目に戻ってきた
    /// 自分の合言葉も同じ扱いになる ([`Marker::Foreign`]、DR-0024 §2)。
    pub fn take_marker(&self, body: &Value) -> Option<Marker> {
        let nonce = nonce_in(body)?;
        let Some(pending) = self.state.lock().unwrap().pending.remove(&nonce) else {
            return Some(Marker::Foreign);
        };
        Some(if Instant::now() <= pending.deadline {
            Marker::Applied
        } else {
            Marker::Late
        })
    }

    /// 合図を 1 つ出す。
    ///
    /// 直前に通った経路が塞がっていたら**出さない**。別の経路へ流れた合図は
    /// upstream にプレフィックスを持たないので、延ばしたい cache には届かず、
    /// 会話に無意味な 1 往復を挟むだけになる (DR-0024 §2)。塞がりは解ける
    /// ものなので、見張りは畳まずに次の予定だけ置き直す。
    async fn fire(self: &Arc<Self>, series: Series, deadline: Instant, deadline_unix: i64) {
        let bound = self
            .state
            .lock()
            .unwrap()
            .watched
            .get(&series)
            .map(|watched| watched.bound.clone());
        let Some(bound) = bound else {
            return;
        };
        if !self.reach.usable(&bound).await {
            debug!(
                session = %series.session_id,
                prefix = %series.prefix,
                route = %bound.route,
                "the route this conversation was cached on is unavailable; not signalling"
            );
            self.rearm(series);
            return;
        }

        let nonce = nonce();
        let notice = events::Keepalive::new(
            now_unix(),
            &series.session_id,
            &series.prefix,
            &nonce,
            deadline_unix,
        );
        let mut state = self.state.lock().unwrap();
        // 予定は使い切った。見張りは続けたまま、戻りを待つ。
        if let Some(watched) = state.watched.get_mut(&series) {
            watched.timer = None;
        }
        state.pending.insert(nonce, Pending { series, deadline });
        drop(state);
        self.events.publish(notice);
    }

    /// 出したまま戻ってきていない合図の数。
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.state.lock().unwrap().pending.len()
    }

    /// 次の合図の予定を持っている系列の数。
    #[cfg(test)]
    fn armed(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .watched
            .values()
            .filter(|watched| watched.timer.is_some())
            .count()
    }
}

/// この 1 本が会話の本流か。
///
/// 道具を渡していないリクエスト (分類器・要約など) は、本流とは別の
/// プレフィックスで走る。そこを起点に合図を出しても、延ばしたい cache は
/// 延びない (DR-0024 §2)。
pub fn carries_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

/// 本文に載っている合言葉。合図の戻りでなければ `None`。
///
/// 探すのは**最後の user メッセージ**の中。合図は通知に包まれて届くことが
/// あるので、ブロックの先頭に来ているとは限らない (= 含んでいれば拾う)。
/// 合言葉は頭 ([`events::KEEPALIVE_TOKEN_PREFIX`]) の後ろに続く、nonce に
/// 使える文字の並び。拾えても、出したものと一致しなければ普通の 1 本になる。
fn nonce_in(body: &Value) -> Option<String> {
    let last = body
        .get("messages")?
        .as_array()?
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))?;
    let texts: Vec<&str> = match last.get("content")? {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => return None,
    };
    for text in texts {
        if let Some(rest) = text.split(events::KEEPALIVE_TOKEN_PREFIX).nth(1) {
            let nonce: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !nonce.is_empty() {
                return Some(nonce);
            }
        }
    }
    None
}

/// 1 回きりの合言葉。32 バイトの乱数を base64url にした 43 文字。
///
/// 推測できると、無関係なリクエストに 1 時間を付けさせられる。OS 由来の
/// 種で回る乱数から起こす ([`crate::credential::oauth`] の token と同じ作り)。
fn nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    rand::fill(&mut bytes);
    B64URL.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Notice;
    use serde_json::json;

    const HORIZON: Duration = Duration::from_secs(8 * 60 * 60);

    fn series() -> Series {
        Series {
            session_id: "s-1".to_owned(),
            prefix: "2cf24dba".to_owned(),
        }
    }

    fn bound() -> Bound {
        Bound {
            ns: "default".to_owned(),
            model: "m".to_owned(),
            route: "a".to_owned(),
        }
    }

    /// 経路が使えるかどうかを、試験の側から切り替えられる口。
    #[derive(Clone, Default)]
    struct Reach(Arc<std::sync::atomic::AtomicBool>);

    impl Reach {
        fn open() -> Self {
            let reach = Self::default();
            reach.set(true);
            reach
        }

        fn set(&self, usable: bool) {
            self.0.store(usable, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Reachable for Reach {
        fn usable<'a>(&'a self, _bound: &'a Bound) -> BoxFuture<'a, bool> {
            let usable = self.0.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { usable })
        }
    }

    fn keepalive() -> (Arc<Keepalive>, tokio::sync::broadcast::Receiver<Notice>) {
        let (keepalive, watching, _) = keepalive_reaching(Reach::open());
        (keepalive, watching)
    }

    fn keepalive_reaching(
        reach: Reach,
    ) -> (
        Arc<Keepalive>,
        tokio::sync::broadcast::Receiver<Notice>,
        Reach,
    ) {
        let events = Arc::new(Events::new());
        let watching = events.subscribe();
        let keepalive = Arc::new(Keepalive::new(events, Arc::new(reach.clone())));
        (keepalive, watching, reach)
    }

    /// 発火したタイマーの続きを走らせる (時計を止めた試験用)。
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// 合図として流れた 1 件。
    fn signalled(notice: Notice) -> events::Keepalive {
        match notice {
            Notice::CacheKeepalive(keepalive) => keepalive,
            other => panic!("expected a keepalive signal, got {other:?}"),
        }
    }

    /// 会話が止まって 55 分で、合図が 1 つ出る。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_that_stops_gets_a_signal() {
        let (keepalive, mut watching) = keepalive();
        let armed_at = now_unix();
        keepalive.armed_by_request(series(), bound(), HORIZON);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        assert_eq!(signal.kind, "cache_keepalive");
        assert_eq!(signal.session_id, "s-1");
        assert_eq!(signal.prefix, "2cf24dba");
        assert!(signal.marker.contains(&signal.nonce));
        assert!(
            signal
                .marker
                .contains(&format!("`LLMGW-KEEPALIVE-{}`", signal.nonce)),
            "the token to send back is spelled out once: {}",
            signal.marker
        );
        assert!(
            (signal.deadline - armed_at - (LIFETIME - MARGIN).as_secs() as i64).abs() <= 1,
            "the deadline is an hour after the request, less the margin"
        );
        assert_eq!(
            signal.deadline_iso,
            crate::credential::time::format_rfc3339(signal.deadline)
        );
        assert_eq!(keepalive.waiting(), 1);
    }

    /// 会話が動いている間は、予定が先送りされて一度も発火しない。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_in_motion_is_never_signalled() {
        let (keepalive, mut watching) = keepalive();

        for _ in 0..5 {
            keepalive.armed_by_request(series(), bound(), HORIZON);
            tokio::time::advance(REFRESH_AFTER - Duration::from_secs(30)).await;
            settle().await;
        }

        assert!(watching.try_recv().is_err(), "nothing was signalled");
        assert_eq!(keepalive.armed(), 1, "one plan, replaced each time");
    }

    /// 期限内に戻ってきた合図は、狙いどおりに効いた 1 本。合言葉は使い切る。
    #[tokio::test(start_paused = true)]
    async fn a_signal_that_comes_back_in_time_is_applied() {
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), HORIZON);
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        let coming_back = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": format!("[SYSTEM NOTIFICATION] {}", signal.marker)},
        ]}]});

        assert_eq!(
            keepalive.take_marker(&coming_back),
            Some(Marker::Applied),
            "found even though the marker is wrapped in a notification"
        );
        assert_eq!(
            keepalive.take_marker(&coming_back),
            Some(Marker::Foreign),
            "a nonce is spent once; what comes back after that is someone else's"
        );
        assert_eq!(keepalive.waiting(), 0);
    }

    /// 期限を過ぎて戻ってきた合図は、繋ぐつもりだった cache に間に合っていない。
    #[tokio::test(start_paused = true)]
    async fn a_signal_that_comes_back_late_is_not_applied() {
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), HORIZON);
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        tokio::time::advance(LIFETIME).await;
        let coming_back = json!({"messages": [{"role": "user", "content": signal.marker}]});

        assert_eq!(keepalive.take_marker(&coming_back), Some(Marker::Late));
    }

    /// 合図が戻ってきた後も、同じ 55 分の間隔で次を出す。
    #[tokio::test(start_paused = true)]
    async fn the_next_signal_follows_at_the_same_interval() {
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), HORIZON);
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        watching.recv().await.unwrap();

        keepalive.rearm(series());
        tokio::time::advance(REFRESH_AFTER - Duration::from_secs(60)).await;
        settle().await;
        assert!(watching.try_recv().is_err(), "not yet");

        tokio::time::advance(Duration::from_secs(120)).await;
        let signal = signalled(watching.recv().await.unwrap());
        assert!(
            signal.deadline - signal.ts >= (LIFETIME - MARGIN - REFRESH_AFTER).as_secs() as i64,
            "the deadline follows the hour this round trip writes"
        );
    }

    /// 実リクエストが最後に来てから horizon を過ぎたら、合図を継ぎ足さない。
    #[tokio::test(start_paused = true)]
    async fn signalling_stops_at_the_horizon() {
        let horizon = Duration::from_secs(2 * 60 * 60);
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), horizon);

        // 合図と応答を、horizon を跨ぐまで繰り返す。
        let mut signals = 0;
        for _ in 0..10 {
            tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
            settle().await;
            if watching.try_recv().is_err() {
                break;
            }
            signals += 1;
            keepalive.rearm(series());
        }

        assert!(
            (2..=4).contains(&signals),
            "{signals} signals before the 2 hour horizon"
        );
        assert_eq!(keepalive.armed(), 0, "no plan is left past the horizon");
        tokio::time::advance(REFRESH_AFTER * 2).await;
        assert!(watching.try_recv().is_err(), "and none fires afterwards");
    }

    /// 直前に通った経路が塞がっている間は、合図を出さない。
    ///
    /// 出しても会話は別の credential へ流れ、延ばしたい cache には届かない。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_whose_route_is_closed_is_not_signalled() {
        let (keepalive, mut watching, reach) = keepalive_reaching(Reach::open());
        reach.set(false);
        keepalive.armed_by_request(series(), bound(), HORIZON);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        settle().await;

        assert!(watching.try_recv().is_err(), "nothing was signalled");
        assert_eq!(keepalive.waiting(), 0, "no nonce was minted either");
        assert_eq!(
            keepalive.armed(),
            1,
            "the watch stays, for the next attempt"
        );
    }

    /// 塞がりが解けたら、次の予定で合図が出る。
    #[tokio::test(start_paused = true)]
    async fn signalling_resumes_once_the_route_reopens() {
        let (keepalive, mut watching, reach) = keepalive_reaching(Reach::open());
        reach.set(false);
        keepalive.armed_by_request(series(), bound(), HORIZON);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        settle().await;
        assert!(watching.try_recv().is_err());

        // 見送りの後は 4 分で次を試す。
        reach.set(true);
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());
        assert_eq!(signal.session_id, "s-1");
    }

    /// 合図を出す前に人が戻ってきたら、予定も合言葉も畳む。
    #[tokio::test(start_paused = true)]
    async fn a_returning_conversation_cancels_what_was_waiting() {
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), HORIZON);
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        keepalive.forget(&series());
        assert_eq!(keepalive.waiting(), 0);
        assert_eq!(keepalive.armed(), 0);

        let coming_back = json!({"messages": [{"role": "user", "content": signal.marker}]});
        assert_eq!(
            keepalive.take_marker(&coming_back),
            Some(Marker::Foreign),
            "a dropped plan cannot be redeemed later"
        );
    }

    /// 合図を出していない系列は、何も持たない。
    #[tokio::test(start_paused = true)]
    async fn forgetting_a_series_that_was_never_armed_is_fine() {
        let (keepalive, _watching) = keepalive();
        keepalive.forget(&series());
        assert_eq!(keepalive.armed(), 0);
    }

    /// 道具を持たない 1 本は会話の本流ではない。
    #[test]
    fn a_request_without_tools_is_not_the_conversation() {
        assert!(carries_tools(&json!({"tools": [{"name": "Bash"}]})));
        for body in [json!({}), json!({"tools": []}), json!({"tools": "none"})] {
            assert!(!carries_tools(&body), "{body}");
        }
    }

    /// 合言葉を持たない本文は、合図の戻りではない。
    #[test]
    fn an_ordinary_request_carries_no_nonce() {
        for body in [
            json!({}),
            json!({"messages": []}),
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            json!({"messages": [{"role": "user", "content": 42}]}),
            json!({"messages": [{"role": "user", "content": "LLMGW-KEEPALIVE- "}]}),
            // 合言葉は最後の user 発話でだけ見る。会話の履歴に残った分は拾わない。
            json!({"messages": [
                {"role": "user", "content": "LLMGW-KEEPALIVE-old"},
                {"role": "assistant", "content": "LLMGW-KEEPALIVE-old"},
                {"role": "user", "content": "and then?"},
            ]}),
        ] {
            assert_eq!(nonce_in(&body), None, "{body}");
        }
    }

    /// 合言葉は毎回違い、長さが決まっていて、URL に置ける文字だけでできている。
    #[test]
    fn each_nonce_is_unpredictable_and_url_safe() {
        let mint: Vec<String> = (0..8).map(|_| nonce()).collect();
        for one in &mint {
            assert_eq!(mint.iter().filter(|other| *other == one).count(), 1);
            assert_eq!(one.len(), 43, "32 bytes as base64url, without padding");
            assert!(
                one.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{one}"
            );
        }
    }

    /// 別のプロセスの合図を見たら、こちらは控えに回る (DR-0024 §2)。
    ///
    /// 相手が生きている限り出さない。相手が居なくなったときだけ引き継ぐ。
    #[tokio::test(start_paused = true)]
    async fn a_signal_from_elsewhere_puts_this_process_on_standby() {
        let (keepalive, mut watching) = keepalive();
        keepalive.standby(series(), bound(), HORIZON);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        settle().await;
        assert!(
            watching.try_recv().is_err(),
            "the other process is still the one signalling"
        );

        tokio::time::advance(STANDBY_AFTER - REFRESH_AFTER).await;
        assert_eq!(
            signalled(watching.recv().await.unwrap()).session_id,
            "s-1",
            "nobody else did it, so this process takes over"
        );
    }

    /// 相手の合図が届き続ける限り、控えは発火しない。
    #[tokio::test(start_paused = true)]
    async fn a_process_on_standby_keeps_stepping_back() {
        let (keepalive, mut watching) = keepalive();

        for _ in 0..6 {
            keepalive.standby(series(), bound(), HORIZON);
            // 相手は 55 分ごとに出す。こちらの控え (57 分) より先に届く。
            tokio::time::advance(REFRESH_AFTER).await;
            settle().await;
            assert!(watching.try_recv().is_err(), "still the other one's turn");
        }
    }

    /// 合図を出した後、何も戻らなければ二度と出さない。
    ///
    /// 戻らないのは、その会話が別のプロセスへ流れた印。出し続けると 2 本に
    /// なるので、次を仕込むのは何かが戻ってきた時だけにする。
    #[tokio::test(start_paused = true)]
    async fn a_signal_nobody_answers_is_not_repeated() {
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), HORIZON);

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        signalled(watching.recv().await.unwrap());

        tokio::time::advance(REFRESH_AFTER * 3).await;
        settle().await;
        assert!(
            watching.try_recv().is_err(),
            "no answer came back, so no second signal goes out"
        );
    }

    /// 見たことのない系列でも控えには入れる。
    ///
    /// フェイルオーバーで初めてその会話を見たプロセスが、そのまま相手の
    /// 後ろに並べる。
    #[tokio::test(start_paused = true)]
    async fn a_series_first_seen_through_a_foreign_signal_can_stand_by() {
        let (keepalive, mut watching) = keepalive();
        assert_eq!(keepalive.armed(), 0);

        keepalive.standby(series(), bound(), HORIZON);
        assert_eq!(keepalive.armed(), 1);

        tokio::time::advance(STANDBY_AFTER + Duration::from_secs(1)).await;
        assert_eq!(signalled(watching.recv().await.unwrap()).session_id, "s-1");
    }
}
