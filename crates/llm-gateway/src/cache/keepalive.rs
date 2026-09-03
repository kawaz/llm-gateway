//! 止まった会話へ、cache を延ばす合図を出す (DR-0024 §2)。
//!
//! 5 分の cache は、会話が止まった 5 分後に消える。消えてから再開すると、
//! その時点のプレフィックス全量を書き直すことになり、書き込みは読み出しの
//! 50 倍の単価で効く。そこで**止まりかけたところで 1 往復だけ挟み**、その
//! 往復にだけ 1 時間を付ける。付くのは差分だけなので、全量の書き直しより安い。
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

use crate::credential::time::now_unix;
use crate::events::{self, Events};

/// 実リクエストの後、合図を出すまで待つ時間。
///
/// 5 分の cache が消える手前。ツールを回している間は数秒おきに次が来るので、
/// ここまで空くこと自体が「止まった」の合図になる。
const AFTER_REQUEST: Duration = Duration::from_secs(4 * 60);

/// 合図が戻ってきた後、次の合図までの時間。付いた cache は 1 時間もつ。
const AFTER_MARKER: Duration = Duration::from_secs(55 * 60);

/// 実リクエストが残した cache の寿命。
const LIFETIME_5M: Duration = Duration::from_secs(5 * 60);

/// 1 時間を付けた合図が残した cache の寿命。
const LIFETIME_1H: Duration = Duration::from_secs(60 * 60);

/// 期限にどれだけ余裕を見るか。
///
/// 合図が届いてから upstream が前処理を始めるまでの分。切り詰めると、
/// 間に合ったつもりの往復が全量の 2 倍書きになる。
const MARGIN: Duration = Duration::from_secs(30);

/// 合言葉の長さ (バイト)。
const NONCE_BYTES: usize = 32;

/// 会話系列。同じ会話でも、系列が違えば別の cache になる (DR-0012 の `prefix`)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Series {
    pub session_id: String,
    pub prefix: String,
}

/// 直前のリクエストが残した cache の寿命。次の合図をいつ出すかが決まる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrote {
    /// 5 分の cache (実リクエスト、および間に合わなかった合図)。
    FiveMinutes,
    /// 1 時間の cache (間に合った合図)。
    OneHour,
}

impl Wrote {
    /// 次の合図を出すまでの間。
    fn rearm_after(self) -> Duration {
        match self {
            Self::FiveMinutes => AFTER_REQUEST,
            Self::OneHour => AFTER_MARKER,
        }
    }

    /// 書いた cache が消えるまでの間。
    fn lifetime(self) -> Duration {
        match self {
            Self::FiveMinutes => LIFETIME_5M,
            Self::OneHour => LIFETIME_1H,
        }
    }
}

/// 戻ってきた合図の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// 期限内。この往復に 1 時間を付ける。
    Applied,
    /// 期限を過ぎていた。付けない — 全量を 2 倍で書くより、1.25 倍の
    /// 書き直しで済ませたほうが安い (DR-0024 §3)。
    Late,
}

impl Marker {
    /// 知らせに出す 1 語。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Late => "late",
        }
    }

    /// この往復が残した cache の寿命。
    ///
    /// 間に合わなかった合図は 5 分の cache しか残していない。次の合図は
    /// 1 時間ではなくその 5 分に合わせる。
    pub fn wrote(self) -> Wrote {
        match self {
            Self::Applied => Wrote::OneHour,
            Self::Late => Wrote::FiveMinutes,
        }
    }
}

/// 合図を出す仕掛け。
pub struct Keepalive {
    events: Arc<Events>,
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
    pub fn new(events: Arc<Events>) -> Self {
        Self {
            events,
            state: Mutex::new(State::default()),
        }
    }

    /// この系列に、次の合図の予定を置き直す。
    ///
    /// 前の予定は捨てる。**次のリクエストが来るたびに先送りされる**ので、
    /// 会話が動いている間は一度も発火しない。
    ///
    /// `horizon` を過ぎた系列には合図を出さない。1 時間の cache を延々と
    /// 継ぎ足す価値があるのは、再開される見込みがある間だけ (DR-0024 §3)。
    pub fn arm(self: &Arc<Self>, series: Series, wrote: Wrote, horizon: Duration) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let horizon_end = match wrote {
            // 実リクエストは、この系列を見張る期間そのものを延ばす。
            Wrote::FiveMinutes => now + horizon,
            // 合図は自分では期間を延ばさない。延ばせるのは人が動かした分だけで、
            // 終わりに達した系列はここで見張るのをやめる。
            Wrote::OneHour => match state.watched.get(&series) {
                Some(watched) if watched.horizon_end > now => watched.horizon_end,
                Some(_) => {
                    state.watched.remove(&series);
                    return;
                }
                None => return,
            },
        };

        let fires_at = now + wrote.rearm_after();
        let deadline = now + wrote.lifetime() - MARGIN;
        let deadline_unix = now_unix() + (wrote.lifetime() - MARGIN).as_secs() as i64;
        let waking = Arc::clone(self);
        let ringing = series.clone();
        let timer = Timer(tokio::spawn(async move {
            tokio::time::sleep_until(fires_at).await;
            waking.fire(ringing, deadline, deadline_unix);
        }));
        // 前の予定は差し替えで畳まれる (`Timer` の Drop が止める)。
        state.watched.insert(
            series,
            Watched {
                timer: Some(timer),
                horizon_end,
            },
        );
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
    /// 合言葉は 1 回だけ有効。同じものが 2 度戻ってきても、2 度目は普通の
    /// リクエストとして扱う。
    pub fn take_marker(&self, body: &Value) -> Option<(Series, Marker)> {
        let nonce = nonce_in(body)?;
        let pending = self.state.lock().unwrap().pending.remove(&nonce)?;
        let marker = if Instant::now() <= pending.deadline {
            Marker::Applied
        } else {
            Marker::Late
        };
        Some((pending.series, marker))
    }

    /// 合図を 1 つ出す。
    fn fire(&self, series: Series, deadline: Instant, deadline_unix: i64) {
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
fn nonce_in(body: &Value) -> Option<String> {
    const OPEN: &str = "[llm-gateway cache keepalive nonce=";

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
        if let Some(rest) = text.split(OPEN).nth(1)
            && let Some(nonce) = rest.split(']').next()
            && !nonce.is_empty()
        {
            return Some(nonce.to_owned());
        }
    }
    None
}

/// 1 回きりの合言葉。
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

    fn keepalive() -> (Arc<Keepalive>, tokio::sync::broadcast::Receiver<Notice>) {
        let events = Arc::new(Events::new());
        let watching = events.subscribe();
        (Arc::new(Keepalive::new(events)), watching)
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

    /// 会話が止まって 4 分で、合図が 1 つ出る。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_that_stops_gets_a_signal() {
        let (keepalive, mut watching) = keepalive();
        let armed_at = now_unix();
        keepalive.arm(series(), Wrote::FiveMinutes, HORIZON);

        tokio::time::advance(AFTER_REQUEST + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        assert_eq!(signal.kind, "cache_keepalive");
        assert_eq!(signal.session_id, "s-1");
        assert_eq!(signal.prefix, "2cf24dba");
        assert!(signal.marker.contains(&signal.nonce));
        assert!(
            signal.marker.contains("reply with exactly"),
            "{}",
            signal.marker
        );
        assert!(
            (signal.deadline - armed_at - (LIFETIME_5M - MARGIN).as_secs() as i64).abs() <= 1,
            "the deadline is the 5 minute cache after the request, less the margin"
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
            keepalive.arm(series(), Wrote::FiveMinutes, HORIZON);
            tokio::time::advance(AFTER_REQUEST - Duration::from_secs(30)).await;
            settle().await;
        }

        assert!(watching.try_recv().is_err(), "nothing was signalled");
        assert_eq!(keepalive.armed(), 1, "one plan, replaced each time");
    }

    /// 期限内に戻ってきた合図には 1 時間を付ける。合言葉は使い切る。
    #[tokio::test(start_paused = true)]
    async fn a_signal_that_comes_back_in_time_is_applied() {
        let (keepalive, mut watching) = keepalive();
        keepalive.arm(series(), Wrote::FiveMinutes, HORIZON);
        tokio::time::advance(AFTER_REQUEST + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        let coming_back = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": format!("[SYSTEM NOTIFICATION] {}", signal.marker)},
        ]}]});

        assert_eq!(
            keepalive.take_marker(&coming_back),
            Some((series(), Marker::Applied)),
            "found even though the marker is wrapped in a notification"
        );
        assert_eq!(
            keepalive.take_marker(&coming_back),
            None,
            "the nonce is spent once"
        );
        assert_eq!(keepalive.waiting(), 0);
    }

    /// 期限を過ぎて戻ってきた合図には付けない。
    #[tokio::test(start_paused = true)]
    async fn a_signal_that_comes_back_late_is_not_applied() {
        let (keepalive, mut watching) = keepalive();
        keepalive.arm(series(), Wrote::FiveMinutes, HORIZON);
        tokio::time::advance(AFTER_REQUEST + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        tokio::time::advance(LIFETIME_5M).await;
        let coming_back = json!({"messages": [{"role": "user", "content": signal.marker}]});

        assert_eq!(
            keepalive.take_marker(&coming_back),
            Some((series(), Marker::Late))
        );
    }

    /// 合図が効いた後は、1 時間の cache に合わせて 55 分後に次を出す。
    #[tokio::test(start_paused = true)]
    async fn after_an_applied_signal_the_next_one_waits_for_the_hour() {
        let (keepalive, mut watching) = keepalive();
        keepalive.arm(series(), Wrote::FiveMinutes, HORIZON);
        tokio::time::advance(AFTER_REQUEST + Duration::from_secs(1)).await;
        watching.recv().await.unwrap();

        keepalive.arm(series(), Wrote::OneHour, HORIZON);
        tokio::time::advance(AFTER_REQUEST + Duration::from_secs(1)).await;
        assert!(
            watching.try_recv().is_err(),
            "four minutes is too early for an hour-long cache"
        );

        tokio::time::advance(AFTER_MARKER - AFTER_REQUEST).await;
        let signal = signalled(watching.recv().await.unwrap());
        assert!(
            signal.deadline - signal.ts >= (LIFETIME_1H - MARGIN - AFTER_MARKER).as_secs() as i64,
            "the deadline follows the hour that was written"
        );
    }

    /// 実リクエストが最後に来てから horizon を過ぎたら、合図を継ぎ足さない。
    #[tokio::test(start_paused = true)]
    async fn signalling_stops_at_the_horizon() {
        let horizon = Duration::from_secs(2 * 60 * 60);
        let (keepalive, mut watching) = keepalive();
        keepalive.arm(series(), Wrote::FiveMinutes, horizon);

        // 合図と応答を、horizon を跨ぐまで繰り返す。
        let mut signals = 0;
        for _ in 0..10 {
            tokio::time::advance(AFTER_MARKER + Duration::from_secs(1)).await;
            settle().await;
            if watching.try_recv().is_err() {
                break;
            }
            signals += 1;
            keepalive.arm(series(), Wrote::OneHour, horizon);
        }

        assert!(
            (2..=4).contains(&signals),
            "{signals} signals before the 2 hour horizon"
        );
        assert_eq!(keepalive.armed(), 0, "no plan is left past the horizon");
        tokio::time::advance(AFTER_MARKER * 2).await;
        assert!(watching.try_recv().is_err(), "and none fires afterwards");
    }

    /// 合図を出す前に人が戻ってきたら、予定も合言葉も畳む。
    #[tokio::test(start_paused = true)]
    async fn a_returning_conversation_cancels_what_was_waiting() {
        let (keepalive, mut watching) = keepalive();
        keepalive.arm(series(), Wrote::FiveMinutes, HORIZON);
        tokio::time::advance(AFTER_REQUEST + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        keepalive.forget(&series());
        assert_eq!(keepalive.waiting(), 0);
        assert_eq!(keepalive.armed(), 0);

        let coming_back = json!({"messages": [{"role": "user", "content": signal.marker}]});
        assert_eq!(
            keepalive.take_marker(&coming_back),
            None,
            "a spent plan cannot be redeemed later"
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
            json!({"messages": [{"role": "user", "content": "[llm-gateway cache keepalive nonce=]"}]}),
            // 合言葉は最後の user 発話でだけ見る。会話の履歴に残った分は拾わない。
            json!({"messages": [
                {"role": "user", "content": "[llm-gateway cache keepalive nonce=old]"},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "and then?"},
            ]}),
        ] {
            assert_eq!(nonce_in(&body), None, "{body}");
        }
    }

    /// 合言葉は毎回違い、URL に置ける文字だけでできている。
    #[test]
    fn each_nonce_is_unpredictable_and_url_safe() {
        let mint: Vec<String> = (0..8).map(|_| nonce()).collect();
        for one in &mint {
            assert_eq!(mint.iter().filter(|other| *other == one).count(), 1);
            assert!(
                one.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{one}"
            );
        }
    }
}
