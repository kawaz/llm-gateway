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
//! 見張りは[置き場]へも落とす。動いている会話なら次のリクエストで張り直るが、
//! **止まっている会話は誰も張り直さない** — そこを繋ぐのが keepalive の
//! 仕事なので、リリースのたびに全部落とすと意味がない。
//!
//! 会話ごとに**合図を止めておく**こともできる (DR-0024 §2 追補)。しばらく
//! 触らないと分かっている会話へ 1 時間おきに合図を出しても、繋がるのは
//! 使われない cache だけ。止めたのは人の意思なので再起動を跨いで残し、
//! 解くのはその会話から実リクエストが来たときだけにする。
//!
//! 出したままの合言葉 (nonce) は落とさない。再起動を跨いで戻ってきた合図は
//! 「出した覚えのない合言葉」= [`Marker::Foreign`] になり、控えとして
//! 吸収される — 別のプロセスの合図と同じ扱いで正しく収束する。
//!
//! [置き場]: store

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info};

use crate::credential::time::{now_unix, now_unix_ms};
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

/// 合言葉の頭に置く乱数の長さ (バイト)。残りは連鎖の起点と終わり。
const NONCE_RANDOM_BYTES: usize = 16;

/// 合言葉に埋めた時刻として認める範囲 (Unix ミリ秒)。
///
/// 32 バイトの乱数はどれも 43 文字の base64url として読めてしまうので、
/// 「時刻として通る値か」で見分ける。両方の欄がこの窓に収まる確率は 1e-14
/// ほどで、乱数を時刻と読み違えることはない。
const TIME_FLOOR_MS: i64 = 1_600_000_000_000;
const TIME_CEIL_MS: i64 = 4_100_000_000_000;

/// 止めた合図を覚えておく長さ。
///
/// 解くのは実リクエストなので、二度と戻らない会話の停止は誰も片付けない。
/// 止めた時点で見張りは畳んであり、停止が効くのは「停止を受け取れなかった
/// 兄弟の合図が `foreign` で見えても控えに入らない」ことだけ。その兄弟も
/// 自分の `keepalive_horizon` が尽きれば黙るので、どの horizon より長く
/// 残せば足りる (horizon に上限は無いが、日単位で書く値ではない)。
const PAUSE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub mod store;

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

/// 合言葉が持ち歩いている、その連鎖の起点と終わり (Unix ミリ秒)。
///
/// 別のプロセスが出した合図を見た側は、控えに入る前にこれを読む。合言葉
/// そのものに書いてあるので、状態を配り合わなくても同じ終わりへ揃う
/// (DR-0024 §2 追補)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    /// この連鎖の起点 = 最後に来た実リクエストの時刻。
    pub since_ms: i64,
    /// この連鎖で合図を出し続ける終わり。
    pub horizon_end_ms: i64,
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

/// 置く予定 1 つ。欄が増えたときに位置で取り違えないよう、名前で書く。
struct Plan {
    series: Series,
    /// 今から合図を出すまでの間。
    after: Duration,
    /// この系列の cache が消える時刻。
    expires_at: Instant,
    horizon_end: Instant,
    bound: Bound,
    kind: store::Kind,
    /// この連鎖の起点と、ここまでに出した本数。
    chain: Counted,
}

/// 合図の連鎖のうち、予定を置き直しても引き継ぐもの。
///
/// 起点が動くのは実リクエストが来たときだけで、そのとき本数も 0 に戻る
/// (= 人が会話を動かしたら、そこから数え直す)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counted {
    /// この連鎖の起点 = 最後に来た実リクエストを送った時刻 (Unix ミリ秒)。
    ///
    /// **知らせに出す `ts` と同じ値**をそのまま持つ。単調時計から欄ごとに
    /// 起こし直すと、同じ瞬間のはずの `ts` と `cache_since` が数ミリ秒ずれ、
    /// 55 分刻みの予定にも端数が乗る (実測)。
    since_ms: i64,
    /// ここまでに出した合図の本数。
    count: u32,
}

/// 合図の間隔と cache の寿命を、知らせの細かさ (ミリ秒) で。
fn refresh_ms() -> i64 {
    REFRESH_AFTER.as_millis() as i64
}

fn lifetime_ms() -> i64 {
    LIFETIME.as_millis() as i64
}

/// 見張っている系列から、次の予定へ引き継ぐもの。
struct Carried {
    horizon_end: Instant,
    bound: Bound,
    chain: Counted,
}

/// この系列に立っている合図の連鎖の見立て (時刻は Unix ミリ秒)。
///
/// 見る側 (ccmsg) が「この会話の cache はいつまで、あと何本の合図で保つか」を
/// 描くための一式 (DR-0012 の `cache_*`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chain {
    /// 連鎖の起点 = 最後に来た実リクエストの時刻 (Unix ミリ秒)。
    pub since_ms: i64,
    /// ここまでに出した合図の本数。実リクエストの直後は 0。
    pub count: u32,
    /// 次の合図の予定時刻 (Unix ミリ秒)。もう出さないなら `None`。
    pub next_at_ms: Option<i64>,
    /// 最後の合図が置く cache が消える時刻 (Unix ミリ秒)。
    pub until_ms: i64,
    /// 連鎖で出す合図の総数。[`Self::until`] を作る合図の番号でもある。
    pub until_count: u32,
}

/// 損益分岐時間から起こした連鎖 (DR-0024 §3)。
///
/// 「合図を出し続ける費用が cache の作り直しに追いつく」までに何本出せて、
/// そこまで繋いだ cache がいつ切れるか。実際に出す本数 ([`Chain::until_count`])
/// と並べると、設定した期間が分岐点の手前か先かが読める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakeven {
    /// 分岐時間に収まる合図の本数。
    pub count: u32,
    /// その最後の 1 本が置く cache が消える時刻 (Unix ミリ秒)。
    pub until_ms: i64,
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

/// この長さの期間に出る合図の本数。
///
/// 期間の終わりを跨いだ 1 本まで出る ([`Keepalive::chain`] と同じ規則)。
/// 期間が [`REFRESH_AFTER`] に満たなくても 1 本は出る — 実リクエストは期間を
/// 見ずに最初の予定を置く ([`Keepalive::armed_by_request`])。
pub fn signals_within(horizon: Duration) -> u32 {
    horizon
        .as_secs()
        .div_ceil(REFRESH_AFTER.as_secs())
        .max(1)
        .min(u32::MAX as u64) as u32
}

/// 合図を出す仕掛け。
pub struct Keepalive {
    events: Arc<Events>,
    /// 直前に通った経路がまだ使えるかを聞く先。
    reach: Arc<dyn Reachable>,
    /// 見張りを再起動を跨いで残す置き場。持たない (= 残さない) 構成もある。
    store: Option<store::Store>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// 見張っている系列。
    watched: HashMap<Series, Watched>,
    /// 出したまま戻ってきていない合図。
    pending: HashMap<String, Pending>,
    /// 合図を止めてある会話と、止めた時刻 (Unix 秒)。
    ///
    /// 鍵が会話の id だけなのは、止めるのが会話全体の意思だから — 系列
    /// (`prefix`) ごとに止めたい場面は無い (DR-0024 §2 追補)。
    paused: HashMap<String, i64>,
}

struct Watched {
    /// 次に合図を出す予定。出した直後は空 (戻りを待っている間)。
    timer: Option<Timer>,
    /// 次に合図を出す時刻。予定を持たない間 (戻り待ち) も、置き場に残すために
    /// 覚えておく。
    fires_at: Instant,
    /// この系列の cache が消える時刻。過ぎていれば張り直す意味がない。
    expires_at: Instant,
    /// この系列に合図を出し続ける終わり。実リクエストのたびに先へ延びる。
    horizon_end: Instant,
    /// 直前の実リクエストが通った先。合図の往復では書き換えない。
    bound: Bound,
    /// 自分が出す番か、別のプロセスの後ろに控えているか。
    kind: store::Kind,
    /// この連鎖の起点と、ここまでに出した本数。
    chain: Counted,
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
            store: None,
            state: Mutex::new(State::default()),
        }
    }

    /// 見張りを残す置き場を持たせる。
    pub fn with_store(mut self, store: store::Store) -> Self {
        self.store = Some(store);
        self
    }

    /// 前回の見張りを読み戻す (DR-0024 §2)。
    ///
    /// 予定の時刻がまだ来ていなければ残りの時間で、過ぎていても cache が
    /// 生きている間なら**すぐに**出す。cache が消えた後・期間の終わった系列は
    /// 捨てる — 出しても繋ぐものが無い。
    pub fn restore(self: &Arc<Self>) {
        let Some(store) = &self.store else {
            return;
        };
        let (now, now_unix) = (Instant::now(), now_unix());
        let kept = store.load();

        // 止めた会話を先に戻す。長く経ちすぎた停止は、解く実リクエストが
        // 二度と来なかったもの — ここで捨てないと誰も片付けない。
        let stale = now_unix - PAUSE_LIFETIME.as_secs() as i64;
        self.state.lock().unwrap().paused = kept
            .paused
            .into_iter()
            .filter(|paused| paused.paused_at > stale)
            .map(|paused| (paused.session_id, paused.paused_at))
            .collect();

        let mut restored = 0;
        for saved in kept.watched {
            if saved.expires_at <= now_unix || saved.horizon_end <= now_unix {
                continue;
            }
            if self.is_paused(&saved.session_id) {
                continue;
            }
            let series = Series {
                session_id: saved.session_id,
                prefix: saved.prefix,
            };
            let bound = Bound {
                ns: saved.ns,
                model: saved.model,
                route: saved.route,
            };
            let after = Duration::from_secs((saved.fires_at - now_unix).max(0) as u64);
            debug!(
                session = %series.session_id,
                prefix = %series.prefix,
                kind = saved.kind.as_str(),
                seconds = after.as_secs(),
                "restoring a cache keepalive watch"
            );
            self.plan(Plan {
                series,
                after,
                expires_at: now + Duration::from_secs((saved.expires_at - now_unix).max(0) as u64),
                horizon_end: now
                    + Duration::from_secs((saved.horizon_end - now_unix).max(0) as u64),
                bound,
                kind: saved.kind,
                chain: Counted {
                    // 起点を持たないファイルでは、予定の 1 つ手前を起点と
                    // 見なす。数え直せる材料が他に無い。
                    since_ms: if saved.since_ms > 0 {
                        saved.since_ms
                    } else {
                        (saved.fires_at - REFRESH_AFTER.as_secs() as i64) * 1_000
                    },
                    count: saved.count,
                },
            });
            restored += 1;
        }
        if restored > 0 {
            info!(series = restored, "picked up the cache keepalive watch");
        }
    }

    /// 今の見張りを置き場へ落とす。
    pub fn save(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let (now, now_unix) = (Instant::now(), now_unix());
        let state = self.state.lock().unwrap();
        let watched: Vec<store::Saved> = state
            .watched
            .iter()
            .map(|(series, watched)| store::Saved {
                session_id: series.session_id.clone(),
                prefix: series.prefix.clone(),
                ns: watched.bound.ns.clone(),
                model: watched.bound.model.clone(),
                route: watched.bound.route.clone(),
                fires_at: unix_of(watched.fires_at, now, now_unix),
                expires_at: unix_of(watched.expires_at, now, now_unix),
                horizon_end: unix_of(watched.horizon_end, now, now_unix),
                since_ms: watched.chain.since_ms,
                count: watched.chain.count,
                kind: watched.kind,
            })
            .collect();
        let paused: Vec<store::Paused> = state
            .paused
            .iter()
            .map(|(session_id, paused_at)| store::Paused {
                session_id: session_id.clone(),
                paused_at: *paused_at,
            })
            .collect();
        drop(state);
        store.save(&store::Kept { watched, paused });
    }

    /// この会話への合図を止める (DR-0024 §2 追補)。
    ///
    /// 会話の id を持つ**全系列**の見張りを畳む。止めたい相手は会話であって
    /// 系列ではないので、どの prefix で走っているかを頼む側に知らせる必要が
    /// ないようにする。合図を出したまま止めた場合、戻ってきた合言葉は
    /// 「出した覚えのない合言葉」= [`Marker::Foreign`] になり、次を仕込まない。
    pub fn pause(&self, session_id: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.paused.insert(session_id.to_owned(), now_unix());
            state
                .watched
                .retain(|series, _| series.session_id != session_id);
            state
                .pending
                .retain(|_, pending| pending.series.session_id != session_id);
        }
        self.save();
    }

    /// この会話への合図を再開する。止まっていなければ何もしない。
    ///
    /// 呼ぶのは**合言葉を持たない実リクエストが来たとき**か、兄弟がそれを
    /// 受けて回してきたときだけ。どちらも人が会話へ戻ってきた印で、これ以外に
    /// 解く口は持たない — 止めた会話へ「まだ止めますか」と合図を出すのは
    /// 本末転倒だし、止めた側が解きに来ることも期待できない。
    ///
    /// 止まっていたのを解いたときだけ真を返す。呼ぶ側はこれを見て、兄弟へ
    /// 解除を回すかどうかを決める。
    pub fn resume(&self, session_id: &str) -> bool {
        if self
            .state
            .lock()
            .unwrap()
            .paused
            .remove(session_id)
            .is_none()
        {
            return false;
        }
        info!(session = session_id, "resuming the cache keepalive signal");
        self.save();
        true
    }

    /// この会話への合図が止まっているか。
    pub fn is_paused(&self, session_id: &str) -> bool {
        self.state.lock().unwrap().paused.contains_key(session_id)
    }

    /// 合図を止めてある会話の id。兄弟へ渡す一覧でもある。
    pub fn paused_sessions(&self) -> Vec<String> {
        let mut sessions: Vec<String> = self.state.lock().unwrap().paused.keys().cloned().collect();
        // 見る側にとって、順番が回るたびに変わる一覧は読みにくい。
        sessions.sort();
        sessions
    }

    /// 実リクエストを送った。見張りを張り直す。
    ///
    /// 前の予定は捨てる。**次のリクエストが来るたびに先送りされる**ので、
    /// 会話が動いている間は一度も発火しない。見張る期間 (`horizon`) と
    /// 通った先を延ばせるのは、この 1 本だけ。
    /// `sent_at_ms` は、この 1 本の知らせに出す `ts` と同じ値を渡す。連鎖の
    /// 起点はそこに揃える (欄ごとに時計を読み直さない)。
    pub fn armed_by_request(
        self: &Arc<Self>,
        series: Series,
        bound: Bound,
        horizon: Duration,
        sent_at_ms: i64,
    ) {
        let now = Instant::now();
        // 人が会話を動かした。連鎖はここから数え直す。
        let chain = Counted {
            since_ms: sent_at_ms,
            count: 0,
        };
        self.schedule(
            series,
            REFRESH_AFTER,
            now + horizon,
            bound,
            store::Kind::Primary,
            chain,
        );
    }

    /// 自分が出した合図が戻ってきた。同じ期間の中で次の予定だけ置き直す。
    ///
    /// 期間を延ばせるのは実リクエストだけなので、`horizon` を過ぎた系列は
    /// ここで見張るのをやめる。1 時間の cache を延々と継ぎ足す価値があるのは、
    /// 再開される見込みがある間だけ (DR-0024 §3)。
    pub fn rearm(self: &Arc<Self>, series: Series) {
        let Some(carried) = self.watch_of(&series) else {
            return;
        };
        self.schedule(
            series,
            REFRESH_AFTER,
            carried.horizon_end,
            carried.bound,
            store::Kind::Primary,
            carried.chain,
        );
    }

    /// 別のプロセスが出した合図を見た。一歩下がって控える (DR-0024 §2)。
    ///
    /// 相手が生きている限り、相手の合図が届くたびにここへ戻ってきて予定が
    /// 後ろへ延びる (= こちらは一度も出さない)。相手が居なくなったときだけ
    /// [`STANDBY_AFTER`] で発火して引き継ぐ。共有する状態を持たずに、
    /// 見えているものだけで 1 本へ収束する。
    ///
    /// この系列を見たことのないプロセスでは、**相手の合言葉に書いてある**
    /// 起点と終わりをそのまま引き継ぐ ([`Signal`])。期間は系列で 1 つなので、
    /// 見ていなかった側がここで数え直すと、2 プロセスが互いの合図を見るたびに
    /// 終わりを作り直して合図が止まらなくなる。終わりを過ぎた合図 (と、
    /// 起点を読めない合図) では控えに入らない — 繋ぐものが残っていない。
    pub fn standby(self: &Arc<Self>, series: Series, bound: Bound, signal: Option<Signal>) {
        let carried = match self.watch_of(&series) {
            Some(carried) => carried,
            None => {
                let Some(signal) = signal else {
                    return;
                };
                let now = Instant::now();
                let left = signal.horizon_end_ms - now_unix_ms();
                if left <= 0 {
                    return;
                }
                Carried {
                    horizon_end: now + Duration::from_millis(left as u64),
                    bound,
                    chain: Counted {
                        since_ms: signal.since_ms,
                        count: 0,
                    },
                }
            }
        };
        self.schedule(
            series,
            STANDBY_AFTER,
            carried.horizon_end,
            carried.bound,
            store::Kind::Standby,
            carried.chain,
        );
    }

    /// 見張っている系列から、次の予定へ引き継ぐもの。期間を過ぎていれば畳む。
    fn watch_of(&self, series: &Series) -> Option<Carried> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        match state.watched.get(series) {
            Some(watched) if watched.horizon_end > now => Some(Carried {
                horizon_end: watched.horizon_end,
                bound: watched.bound.clone(),
                chain: watched.chain,
            }),
            Some(_) => {
                state.watched.remove(series);
                drop(state);
                self.save();
                None
            }
            None => None,
        }
    }

    /// 次に合図を出す時刻を置く。
    ///
    /// 止めてある会話には何も置かない。控え ([`Self::standby`]) も置かないのは、
    /// 別のプロセスの合図が見えたからといって引き継ぐ相手が居ないため — 止めた
    /// のは会話全体の意思で、兄弟にも同じ停止が渡っている (DR-0024 §2 追補)。
    fn schedule(
        self: &Arc<Self>,
        series: Series,
        after: Duration,
        horizon_end: Instant,
        bound: Bound,
        kind: store::Kind,
        chain: Counted,
    ) {
        if self.is_paused(&series.session_id) {
            return;
        }
        let now = Instant::now();
        self.plan(Plan {
            series,
            after,
            // この 1 本が置いた cache が消える時刻。合図が間に合ったかの
            // 判定にも、置き場から読み戻すかの判定にも使う。
            expires_at: now + LIFETIME - MARGIN,
            horizon_end,
            bound,
            kind,
            chain,
        });
    }

    /// 予定を 1 つ置いて、置き場へ落とす。
    fn plan(self: &Arc<Self>, plan: Plan) {
        let now = Instant::now();
        let fires_at = now + plan.after;
        let expires_at = plan.expires_at;
        let expires_at_ms = unix_ms_of(expires_at, now, now_unix_ms());
        let waking = Arc::clone(self);
        let ringing = plan.series.clone();
        let timer = Timer(tokio::spawn(async move {
            tokio::time::sleep_until(fires_at).await;
            waking.fire(ringing, expires_at, expires_at_ms).await;
        }));
        // 前の予定は差し替えで畳まれる (`Timer` の Drop が止める)。
        self.state.lock().unwrap().watched.insert(
            plan.series,
            Watched {
                timer: Some(timer),
                fires_at,
                expires_at,
                horizon_end: plan.horizon_end,
                bound: plan.bound,
                kind: plan.kind,
                chain: plan.chain,
            },
        );
        self.save();
    }

    /// この系列に立っている合図の連鎖 (DR-0012 の `cache_*`)。見張っていなければ
    /// `None`。
    ///
    /// 合図は今の予定 ([`Watched::fires_at`]) から [`REFRESH_AFTER`] 刻みで
    /// 続き、次を仕込めるのは**その 1 本を出す時点で見張る期間が残っている**
    /// 間だけ ([`Self::rearm`] → [`Self::watch_of`] の `horizon_end > now`)。
    /// つまり期間の終わりを跨いだ 1 本が最後に出て、そこから [`LIFETIME`] が
    /// この系列の cache の終わりになる。
    ///
    /// **見込み値**。合図が出せない (経路が塞がる) / 戻りが遅れて 1 時間が
    /// 付かない場合は、ここより早く切れる。
    pub fn chain(&self, series: &Series) -> Option<Chain> {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        let watched = state.watched.get(series)?;
        let (fires_at, horizon_end, counted) =
            (watched.fires_at, watched.horizon_end, watched.chain);
        // 予定を持たない間 (= 出した 1 本の戻り待ち) の次は、戻ってきたときに
        // 置き直される 1 本。それが仕込まれるのは、そのとき期間が残っている
        // 場合だけ ([`Self::rearm`])。
        let planned = watched.timer.is_some();
        let has_next = planned || horizon_end > now;
        drop(state);

        // 時刻は起点からの**整数演算**だけで出す。合図は 55 分の格子に乗る
        // ので、単調時計から起こし直すと端数が乗るだけで何も得られない。
        let at = |signal: u32| counted.since_ms + signal as i64 * refresh_ms();
        if !has_next {
            // 出した 1 本が最後。それが置く cache で終わる。
            return Some(Chain {
                since_ms: counted.since_ms,
                count: counted.count,
                next_at_ms: None,
                until_ms: at(counted.count) + lifetime_ms(),
                until_count: counted.count,
            });
        }
        // 次の 1 本の後に、あと何本続くか。期間の終わりちょうどに出る 1 本は、
        // その時点で期間が残っていない (`>` の比較) ので出ない。本数だけを
        // 単調時計から数え、時刻には持ち込まない。
        let next_at = if planned {
            fires_at
        } else {
            fires_at + REFRESH_AFTER
        };
        let more = horizon_end
            .checked_duration_since(next_at)
            .map_or(0, |left| left.as_secs().div_ceil(REFRESH_AFTER.as_secs()))
            as u32;
        let until_count = counted.count + 1 + more;
        Some(Chain {
            since_ms: counted.since_ms,
            count: counted.count,
            next_at_ms: Some(at(counted.count + 1)),
            until_ms: at(until_count) + lifetime_ms(),
            until_count,
        })
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
        {
            let mut state = self.state.lock().unwrap();
            state.watched.remove(series);
            state.pending.retain(|_, pending| &pending.series != series);
        }
        self.save();
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
    async fn fire(self: &Arc<Self>, series: Series, deadline: Instant, deadline_ms: i64) {
        let watching = self
            .state
            .lock()
            .unwrap()
            .watched
            .get(&series)
            .map(|watched| {
                (
                    watched.bound.clone(),
                    watched.chain.since_ms,
                    watched.horizon_end,
                )
            });
        let Some((bound, since_ms, horizon_end)) = watching else {
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

        // 合言葉には、この連鎖の起点と終わりを持たせる。受け取った別の
        // プロセスは、これを読んで同じ終わりの控えに入る (DR-0024 §2 追補)。
        let (now, now_ms) = (Instant::now(), now_unix_ms());
        let nonce = nonce(Signal {
            since_ms,
            horizon_end_ms: unix_ms_of(horizon_end, now, now_ms),
        });
        let notice = events::Keepalive::new(
            now_ms,
            &series.session_id,
            &series.prefix,
            &nonce,
            deadline_ms,
        );
        let mut state = self.state.lock().unwrap();
        // 予定は使い切った。見張りは続けたまま、戻りを待つ。
        if let Some(watched) = state.watched.get_mut(&series) {
            watched.timer = None;
            watched.chain.count += 1;
        }
        state.pending.insert(nonce, Pending { series, deadline });
        drop(state);
        self.save();
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

/// 単調時計の時刻を、置き場に書ける時刻へ直す。
///
/// 単調時計は保存できない (再起動で起点が変わる)。読み書きの瞬間の対応
/// 1 組だけを使って差で写す。
fn unix_of(instant: Instant, now: Instant, now_unix: i64) -> i64 {
    if instant >= now {
        now_unix + (instant - now).as_secs() as i64
    } else {
        now_unix - (now - instant).as_secs() as i64
    }
}

/// 単調時計の時刻を、知らせに出せる Unix ミリ秒へ直す。
///
/// [`unix_of`] と同じ写し方 (読み書きの瞬間の対応 1 組を使った差) を、
/// 知らせの細かさ (ミリ秒) で行う。
fn unix_ms_of(instant: Instant, now: Instant, now_ms: i64) -> i64 {
    if instant >= now {
        now_ms + (instant - now).as_millis() as i64
    } else {
        now_ms - (now - instant).as_millis() as i64
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

/// 本文に載っている合言葉から読める、連鎖の起点と終わり。
///
/// 別のプロセスが出した合図を受けた側が、控えに入る前に読む
/// ([`Keepalive::standby`])。時刻として通らない値 (= 別の作りの合言葉) では
/// `None`。
pub fn signal_in(body: &Value) -> Option<Signal> {
    signal_of(&nonce_in(body)?)
}

/// 1 回きりの合言葉。32 バイトを base64url にした 43 文字。
///
/// 中身は 16 バイトの乱数と、この連鎖の起点・終わり (それぞれ Unix ミリ秒を
/// 8 バイトの big endian で)。合言葉に書いておけば、初めてこの会話を見た
/// プロセスも状態を配ってもらわずに同じ終わりへ揃えられる (DR-0024 §2 追補)。
///
/// 推測できると、無関係なリクエストに 1 時間を付けさせられる。乱数は OS 由来
/// の種で回る ([`crate::credential::oauth`] の token と同じ作り)。時刻の 16
/// バイトは推測できるが、残る 128 ビットは総当たりできる量ではない。
fn nonce(signal: Signal) -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    rand::fill(&mut bytes[..NONCE_RANDOM_BYTES]);
    bytes[NONCE_RANDOM_BYTES..NONCE_RANDOM_BYTES + 8]
        .copy_from_slice(&signal.since_ms.to_be_bytes());
    bytes[NONCE_RANDOM_BYTES + 8..].copy_from_slice(&signal.horizon_end_ms.to_be_bytes());
    B64URL.encode(bytes)
}

/// 別のプロセスが出したことにする合言葉 (この crate の試験用)。
#[cfg(test)]
pub(crate) fn foreign_nonce(since_ms: i64, horizon: Duration) -> String {
    nonce(Signal {
        since_ms,
        horizon_end_ms: since_ms + horizon.as_millis() as i64,
    })
}

/// 合言葉に書いてある起点と終わり。読めなければ `None`。
fn signal_of(nonce: &str) -> Option<Signal> {
    let bytes = B64URL.decode(nonce).ok()?;
    let bytes: [u8; NONCE_BYTES] = bytes.try_into().ok()?;
    let at = |from: usize| i64::from_be_bytes(bytes[from..from + 8].try_into().unwrap());
    let signal = Signal {
        since_ms: at(NONCE_RANDOM_BYTES),
        horizon_end_ms: at(NONCE_RANDOM_BYTES + 8),
    };
    let sane = |ms: i64| (TIME_FLOOR_MS..=TIME_CEIL_MS).contains(&ms);
    (sane(signal.since_ms)
        && sane(signal.horizon_end_ms)
        && signal.horizon_end_ms >= signal.since_ms)
        .then_some(signal)
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

    /// 相手が出した合図が持ってくる、連鎖の起点と終わり。
    fn signal(horizon: Duration) -> Option<Signal> {
        let since_ms = now_unix_ms();
        Some(Signal {
            since_ms,
            horizon_end_ms: since_ms + horizon.as_millis() as i64,
        })
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
        let armed_at_ms = now_unix_ms();
        keepalive.armed_by_request(series(), bound(), HORIZON, armed_at_ms);

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
            "the nonce to send back is spelled out once: {}",
            signal.marker
        );
        assert!(
            (signal.deadline - armed_at_ms - (LIFETIME - MARGIN).as_millis() as i64).abs() <= 1_000,
            "the deadline is an hour after the request, less the margin (in milliseconds)"
        );
        assert!(
            signal.ts > 1_700_000_000_000,
            "the signal's own time is in milliseconds: {}",
            signal.ts
        );
        assert_eq!(keepalive.waiting(), 1);
    }

    /// 会話が動いている間は、予定が先送りされて一度も発火しない。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_in_motion_is_never_signalled() {
        let (keepalive, mut watching) = keepalive();

        for _ in 0..5 {
            keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
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
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
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
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
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
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        watching.recv().await.unwrap();

        keepalive.rearm(series());
        tokio::time::advance(REFRESH_AFTER - Duration::from_secs(60)).await;
        settle().await;
        assert!(watching.try_recv().is_err(), "not yet");

        tokio::time::advance(Duration::from_secs(120)).await;
        let signal = signalled(watching.recv().await.unwrap());
        assert!(
            signal.deadline - signal.ts >= (LIFETIME - MARGIN - REFRESH_AFTER).as_millis() as i64,
            "the deadline follows the hour this round trip writes"
        );
    }

    /// 実リクエストが最後に来てから horizon を過ぎたら、合図を継ぎ足さない。
    #[tokio::test(start_paused = true)]
    async fn signalling_stops_at_the_horizon() {
        let horizon = Duration::from_secs(2 * 60 * 60);
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), horizon, now_unix_ms());

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

    /// 継ぎ足せる終わりは、期間を跨いだ最後の 1 本が置く cache の終わり。
    ///
    /// 次を仕込めるのは「その 1 本を出す時点で期間が残っている」間なので、
    /// 期間の終わりちょうどでは仕込まれず、跨いだ 1 本が最後になる。
    #[tokio::test(start_paused = true)]
    async fn the_end_is_the_hour_the_last_signal_buys() {
        for (horizon_minutes, signals) in [
            // 55 分に満たない期間でも、最初の 1 本は必ず出る (実リクエストは
            // 期間を見ずに仕込む)。
            (0, 1),
            (30, 1),
            // 55 分の倍数ちょうど。その時刻には期間が残っていないので、
            // そこで打ち切られる。
            (55, 1),
            (110, 2),
            // 端数。期間を跨いだ 1 本が最後に出る。
            (120, 3),
            (8 * 60, 9),
        ] {
            let (keepalive, _watching) = keepalive();
            let armed_at_ms = now_unix_ms();
            let horizon = Duration::from_secs(horizon_minutes * 60);
            keepalive.armed_by_request(series(), bound(), horizon, armed_at_ms);

            let chain = keepalive.chain(&series()).unwrap();
            let want = armed_at_ms + (REFRESH_AFTER * signals + LIFETIME).as_millis() as i64;
            assert_eq!(chain.until_count, signals, "over {horizon_minutes} minutes");
            assert_eq!(
                chain.until_ms, want,
                "a {horizon_minutes} minute horizon ends on the 55 minute grid"
            );
            assert_eq!(
                signals_within(horizon),
                signals,
                "the same count comes out of the horizon alone"
            );

            // 実リクエスト自身は連鎖の 0 番目で、次の 1 本はきっかり 55 分後。
            assert_eq!(chain.count, 0);
            assert_eq!(
                chain.since_ms, armed_at_ms,
                "the start is the very moment the request went out"
            );
            assert_eq!(
                chain.next_at_ms,
                Some(armed_at_ms + REFRESH_AFTER.as_millis() as i64)
            );
        }
    }

    /// 数えた終わりと、実際に出る合図の本数が食い違わない。
    ///
    /// 終わりは予定から数えた見込みなので、見張りの側の条件と揃っている
    /// ことを、出た本数で確かめる。連鎖の番号も 1 本ごとに 1 つ進む。
    #[tokio::test(start_paused = true)]
    async fn the_end_agrees_with_how_many_signals_actually_go_out() {
        let horizon = Duration::from_secs(2 * 60 * 60);
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), horizon, now_unix_ms());
        let promised = keepalive.chain(&series()).unwrap();

        let mut signals = 0;
        for _ in 0..10 {
            tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
            settle().await;
            if watching.try_recv().is_err() {
                break;
            }
            signals += 1;
            // 合図が出た直後の連鎖は、その 1 本まで数え終わっている。
            let waiting = keepalive.chain(&series()).unwrap();
            assert_eq!(waiting.count, signals, "the chain counts each signal");
            assert_eq!(waiting.until_count, promised.until_count, "the end holds");
            assert_eq!(waiting.since_ms, promised.since_ms, "the start never moves");
            assert_eq!(waiting.until_ms, promised.until_ms, "nor does the end");
            // 最後の 1 本を出し終えたときだけ次が無い (下で確かめる)。
            if let Some(next_at_ms) = waiting.next_at_ms {
                assert_eq!(
                    next_at_ms,
                    promised.since_ms + (signals as i64 + 1) * REFRESH_AFTER.as_millis() as i64,
                    "each signal sits on the 55 minute grid"
                );
            }

            keepalive.rearm(series());
            // 置き直した後も同じ終わりを指す。期間が尽きていれば見張りごと
            // 畳まれ、そこで連鎖も終わる。
            if let Some(rearmed) = keepalive.chain(&series()) {
                assert_eq!(rearmed.count, signals);
                assert_eq!(rearmed.until_count, promised.until_count);
                assert_eq!(rearmed.until_ms, promised.until_ms);
                assert_eq!(rearmed.since_ms, promised.since_ms);
            } else {
                assert_eq!(
                    waiting.next_at_ms, None,
                    "the last signal is told apart by having no next"
                );
            }
        }
        assert_eq!(
            signals, promised.until_count,
            "{} signals were promised",
            promised.until_count
        );
    }

    /// 分岐点は、実際の連鎖と同じ数え方で起こす (DR-0024 §3)。
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

    /// 見張っていない系列には、連鎖が無い。
    #[tokio::test(start_paused = true)]
    async fn a_series_nobody_watches_has_no_chain_to_report() {
        let (keepalive, _watching) = keepalive();
        assert_eq!(keepalive.chain(&series()), None);

        keepalive.pause("s-1");
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        assert_eq!(
            keepalive.chain(&series()),
            None,
            "a paused conversation is not signalled, so nothing is extended"
        );
    }

    /// 連鎖の起点と本数は、再起動を跨いでも続く。
    #[tokio::test(start_paused = true)]
    async fn the_chain_keeps_its_place_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (before, mut watching) = keepalive_storing(dir.path());
        before.armed_by_request(series(), bound(), HORIZON, now_unix_ms());

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        signalled(watching.recv().await.unwrap());
        before.rearm(series());
        // 時計を止めた試験では単調時計だけが進むので、起点は「進めた後」に
        // 読んだ両者で比べる (実機では壁時計も一緒に進む)。
        let started = before.chain(&series()).unwrap();
        assert_eq!(started.count, 1);
        drop(before);

        let (after, _) = keepalive_storing(dir.path());
        after.restore();
        let picked_up = after.chain(&series()).unwrap();
        assert_eq!(picked_up.count, 1, "the signal that went out still counts");
        assert_eq!(
            picked_up.since_ms, started.since_ms,
            "and the start comes back as the very same moment"
        );
    }

    /// 直前に通った経路が塞がっている間は、合図を出さない。
    ///
    /// 出しても会話は別の credential へ流れ、延ばしたい cache には届かない。
    #[tokio::test(start_paused = true)]
    async fn a_conversation_whose_route_is_closed_is_not_signalled() {
        let (keepalive, mut watching, reach) = keepalive_reaching(Reach::open());
        reach.set(false);
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());

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
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());

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
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
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
        let carried = signal(HORIZON).unwrap();
        let mint: Vec<String> = (0..8).map(|_| nonce(carried)).collect();
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

    /// 合言葉は、連鎖の起点と終わりをそのまま持ち帰る。
    ///
    /// 別のプロセスはこれだけを頼りに控えへ入るので、往復して同じ値が読める
    /// ことがこの仕掛けの土台になる (DR-0024 §2 追補)。
    #[test]
    fn a_nonce_carries_the_chain_it_belongs_to() {
        for carried in [
            Signal {
                since_ms: 1_800_000_000_000,
                horizon_end_ms: 1_800_000_000_000 + HORIZON.as_millis() as i64,
            },
            // 起点と終わりが同じ (期間 0) 合言葉も読める。
            Signal {
                since_ms: TIME_FLOOR_MS,
                horizon_end_ms: TIME_FLOOR_MS,
            },
        ] {
            let minted = nonce(carried);
            assert_eq!(minted.len(), 43, "the shape does not change");
            assert_eq!(signal_of(&minted), Some(carried));
        }
    }

    /// 時刻を持たない合言葉は読めない。
    ///
    /// 別の作りの合言葉 (32 バイトの乱数) を時刻と読み違えると、でたらめな
    /// 終わりの控えができる。
    #[test]
    fn a_nonce_without_a_chain_in_it_is_not_read() {
        let mut random = [0u8; NONCE_BYTES];
        rand::fill(&mut random);
        for unreadable in [
            B64URL.encode(random),
            B64URL.encode([0u8; NONCE_BYTES]),
            "not-base64url!".to_owned(),
            // 長さが違う。
            B64URL.encode([7u8; 16]),
            // 終わりが起点より手前。
            B64URL.encode({
                let mut bytes = [0u8; NONCE_BYTES];
                bytes[16..24].copy_from_slice(&(TIME_FLOOR_MS + 1).to_be_bytes());
                bytes[24..].copy_from_slice(&TIME_FLOOR_MS.to_be_bytes());
                bytes
            }),
        ] {
            assert_eq!(signal_of(&unreadable), None, "{unreadable}");
        }
    }

    /// 相手の合図を初めて見たプロセスは、相手の終わりをそのまま引き継ぐ。
    ///
    /// ここで期間を数え直すと、2 つのプロセスが互いの合図を見るたびに終わりを
    /// 作り直し、実リクエストが 1 本も来ないまま合図が続く (DR-0024 §2 追補)。
    #[tokio::test(start_paused = true)]
    async fn standing_by_keeps_the_end_the_other_process_set() {
        let (signalling, mut watching) = keepalive();
        signalling.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let sent = signalled(watching.recv().await.unwrap());
        let carried = signal_of(&sent.nonce).expect("the signal carries its chain");

        // 同じ会話を見ている別のプロセス。この系列は初めて見る。
        let (standing_by, _) = keepalive();
        standing_by.standby(series(), bound(), Some(carried));

        let chain = standing_by.chain(&series()).unwrap();
        assert_eq!(
            chain.since_ms, carried.since_ms,
            "the start comes from the other process, not from this one's clock"
        );
        assert_eq!(
            chain.until_ms,
            carried.since_ms + (REFRESH_AFTER * chain.until_count + LIFETIME).as_millis() as i64,
        );
        assert!(
            // 期間を跨いだ 1 本が最後に出るので、終わりは 1 本分だけ先に伸びる。
            chain.until_ms <= carried.horizon_end_ms + lifetime_ms() + refresh_ms(),
            "and the end stays the one the other process set"
        );
    }

    /// 終わりを過ぎた合図では、控えに入らない。
    ///
    /// 期間の尽きた会話へ引き継ぐものは無い。読めない合言葉も同じ扱い。
    #[tokio::test(start_paused = true)]
    async fn a_signal_past_its_end_does_not_put_anyone_on_standby() {
        let since_ms = now_unix_ms() - 2 * HORIZON.as_millis() as i64;
        for spent in [
            Some(Signal {
                since_ms,
                horizon_end_ms: since_ms + HORIZON.as_millis() as i64,
            }),
            // 読めない合言葉。
            None,
        ] {
            let (keepalive, mut watching) = keepalive();
            keepalive.standby(series(), bound(), spent);
            assert_eq!(keepalive.armed(), 0, "nothing is left to hand over");

            tokio::time::advance(STANDBY_AFTER * 2).await;
            settle().await;
            assert!(watching.try_recv().is_err(), "so nothing is signalled");
        }
    }

    /// 別のプロセスの合図を見たら、こちらは控えに回る (DR-0024 §2)。
    ///
    /// 相手が生きている限り出さない。相手が居なくなったときだけ引き継ぐ。
    #[tokio::test(start_paused = true)]
    async fn a_signal_from_elsewhere_puts_this_process_on_standby() {
        let (keepalive, mut watching) = keepalive();
        keepalive.standby(series(), bound(), signal(HORIZON));

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
            keepalive.standby(series(), bound(), signal(HORIZON));
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
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());

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

        keepalive.standby(series(), bound(), signal(HORIZON));
        assert_eq!(keepalive.armed(), 1);

        tokio::time::advance(STANDBY_AFTER + Duration::from_secs(1)).await;
        assert_eq!(signalled(watching.recv().await.unwrap()).session_id, "s-1");
    }

    /// 停止を持たない、見張りだけの一式。
    fn kept(watched: Vec<store::Saved>) -> store::Kept {
        store::Kept {
            watched,
            paused: Vec::new(),
        }
    }

    /// 止めた会話には合図を仕込まない (DR-0024 §2 追補)。
    ///
    /// 実リクエストでも、別のプロセスの合図でも、合図の往復でも同じ。
    #[tokio::test(start_paused = true)]
    async fn a_paused_conversation_is_never_armed() {
        let (keepalive, mut watching) = keepalive();
        keepalive.pause("s-1");

        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        assert_eq!(keepalive.armed(), 0, "a real request does not arm it");

        keepalive.standby(series(), bound(), signal(HORIZON));
        assert_eq!(
            keepalive.armed(),
            0,
            "nor does someone else's signal — the pause reached them too"
        );

        keepalive.rearm(series());
        assert_eq!(keepalive.armed(), 0);

        tokio::time::advance(STANDBY_AFTER * 2).await;
        settle().await;
        assert!(watching.try_recv().is_err(), "so nothing is ever signalled");
    }

    /// 止めると、その会話の系列は全部畳まれる。
    ///
    /// 止めたいのは会話であって系列ではないので、頼む側がどの prefix で
    /// 走っているかを知っている必要はない。
    #[tokio::test(start_paused = true)]
    async fn pausing_folds_every_series_of_that_conversation() {
        let (keepalive, _watching) = keepalive();
        let other_prefix = Series {
            session_id: "s-1".to_owned(),
            prefix: "9e107d9d".to_owned(),
        };
        let other_session = Series {
            session_id: "s-2".to_owned(),
            prefix: "2cf24dba".to_owned(),
        };
        for series in [series(), other_prefix, other_session.clone()] {
            keepalive.armed_by_request(series, bound(), HORIZON, now_unix_ms());
        }
        assert_eq!(keepalive.armed(), 3);

        keepalive.pause("s-1");
        assert_eq!(keepalive.armed(), 1, "only the other conversation is left");
        assert_eq!(keepalive.bound(&other_session), Some(bound()));
    }

    /// 合言葉を持たない実リクエストが、止めた合図を解く。
    #[tokio::test(start_paused = true)]
    async fn a_real_request_lifts_the_pause() {
        let (keepalive, mut watching) = keepalive();
        keepalive.pause("s-1");

        keepalive.resume(&series().session_id);
        assert!(!keepalive.is_paused("s-1"));
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        assert_eq!(signalled(watching.recv().await.unwrap()).session_id, "s-1");
    }

    /// 合図の往復は解除に数えない。
    ///
    /// 止めた会話へ最後の合図が出たままだったときに、その戻りで自分を解いて
    /// しまうと、止めた意味がなくなる。
    #[tokio::test(start_paused = true)]
    async fn a_signal_coming_back_does_not_lift_the_pause() {
        let (keepalive, mut watching) = keepalive();
        keepalive.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());

        keepalive.pause("s-1");
        let coming_back = json!({"messages": [{"role": "user", "content": signal.marker}]});
        assert_eq!(
            keepalive.take_marker(&coming_back),
            Some(Marker::Foreign),
            "the nonce went away with the pause"
        );
        assert!(keepalive.is_paused("s-1"), "and the pause is still there");

        keepalive.rearm(series());
        tokio::time::advance(STANDBY_AFTER * 2).await;
        settle().await;
        assert!(watching.try_recv().is_err());
    }

    /// 止めてある会話の一覧は、名前の順で返る。
    #[tokio::test(start_paused = true)]
    async fn the_paused_conversations_come_back_in_a_settled_order() {
        let (keepalive, _watching) = keepalive();
        assert!(keepalive.paused_sessions().is_empty());

        for session in ["s-3", "s-1", "s-2"] {
            keepalive.pause(session);
        }
        assert_eq!(keepalive.paused_sessions(), ["s-1", "s-2", "s-3"]);
    }

    /// 置き場を持たせた見張り。
    fn keepalive_storing(
        dir: &std::path::Path,
    ) -> (Arc<Keepalive>, tokio::sync::broadcast::Receiver<Notice>) {
        let events = Arc::new(Events::new());
        let watching = events.subscribe();
        let keepalive = Keepalive::new(events, Arc::new(Reach::open()))
            .with_store(store::Store::new(dir, "127.0.0.1:11301"));
        (Arc::new(keepalive), watching)
    }

    /// 止まっている会話の見張りは、再起動を跨いで残る (DR-0024 §2)。
    ///
    /// 動いている会話なら次のリクエストで張り直るが、止まっている会話は
    /// 誰も張り直さない。
    #[tokio::test(start_paused = true)]
    async fn the_watch_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (before, _) = keepalive_storing(dir.path());
        before.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        drop(before);

        // ここで落ちて、起動し直す。
        let (after, mut watching) = keepalive_storing(dir.path());
        after.restore();
        assert_eq!(after.armed(), 1, "the watch was picked up");

        tokio::time::advance(REFRESH_AFTER + Duration::from_secs(1)).await;
        let signal = signalled(watching.recv().await.unwrap());
        assert_eq!(signal.session_id, "s-1");
        assert_eq!(signal.prefix, "2cf24dba");
    }

    /// 予定の時刻を過ぎていても、cache が生きている間はすぐに出す。
    #[tokio::test(start_paused = true)]
    async fn a_watch_that_came_due_while_down_signals_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::new(dir.path(), "127.0.0.1:11301");
        let now = now_unix();
        store.save(&kept(vec![store::Saved {
            session_id: "s-1".to_owned(),
            prefix: "2cf24dba".to_owned(),
            ns: "default".to_owned(),
            model: "m".to_owned(),
            route: "a".to_owned(),
            // 止まっている間に予定の時刻が過ぎた。cache はまだ生きている。
            fires_at: now - 60,
            expires_at: now + 120,
            horizon_end: now + 3600,
            since_ms: now * 1_000,
            count: 0,
            kind: store::Kind::Primary,
        }]));

        let (keepalive, mut watching) = keepalive_storing(dir.path());
        keepalive.restore();

        settle().await;
        assert_eq!(
            signalled(watching.recv().await.unwrap()).session_id,
            "s-1",
            "the remaining life is worth one signal right away"
        );
    }

    /// cache の消えた系列と、期間の終わった系列は読み戻さない。
    #[tokio::test(start_paused = true)]
    async fn a_watch_with_nothing_left_to_extend_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::new(dir.path(), "127.0.0.1:11301");
        let now = now_unix();
        let saved = |expires_at: i64, horizon_end: i64| store::Saved {
            session_id: "s-1".to_owned(),
            prefix: "2cf24dba".to_owned(),
            ns: "default".to_owned(),
            model: "m".to_owned(),
            route: "a".to_owned(),
            fires_at: now + 60,
            expires_at,
            horizon_end,
            since_ms: now * 1_000,
            count: 0,
            kind: store::Kind::Primary,
        };

        for gone in [
            // cache が消えている。
            saved(now - 1, now + 3600),
            // 見張る期間が終わっている。
            saved(now + 120, now - 1),
        ] {
            store.save(&kept(vec![gone]));
            let (keepalive, _watching) = keepalive_storing(dir.path());
            keepalive.restore();
            assert_eq!(keepalive.armed(), 0);
        }
    }

    /// 止めた合図は再起動を跨いで残り、見張りも読み戻さない。
    ///
    /// 止めたのは人の意思で、再起動はそれを覆す出来事ではない。
    #[tokio::test(start_paused = true)]
    async fn a_pause_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (before, _) = keepalive_storing(dir.path());
        before.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        before.pause("s-1");
        drop(before);

        let (after, mut watching) = keepalive_storing(dir.path());
        after.restore();
        assert!(after.is_paused("s-1"));
        assert_eq!(after.armed(), 0);

        tokio::time::advance(STANDBY_AFTER * 2).await;
        settle().await;
        assert!(watching.try_recv().is_err());
    }

    /// 見張りが残っていても、止めてある会話の分は読み戻さない。
    ///
    /// 止めた側と見張りを書いた側が別のプロセスだと、この形になる。
    #[tokio::test(start_paused = true)]
    async fn a_watch_belonging_to_a_paused_conversation_is_not_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::new(dir.path(), "127.0.0.1:11301");
        let now = now_unix();
        store.save(&store::Kept {
            watched: vec![store::Saved {
                session_id: "s-1".to_owned(),
                prefix: "2cf24dba".to_owned(),
                ns: "default".to_owned(),
                model: "m".to_owned(),
                route: "a".to_owned(),
                fires_at: now + 60,
                expires_at: now + 3000,
                horizon_end: now + 3600,
                since_ms: now * 1_000,
                count: 0,
                kind: store::Kind::Primary,
            }],
            paused: vec![store::Paused {
                session_id: "s-1".to_owned(),
                paused_at: now - 60,
            }],
        });

        let (keepalive, _watching) = keepalive_storing(dir.path());
        keepalive.restore();
        assert_eq!(keepalive.armed(), 0);
    }

    /// 誰も解きに来なかった停止は、起動時に捨てる。
    ///
    /// 解くのは実リクエストだけなので、二度と戻らない会話の停止は放って
    /// おくと溜まり続ける。
    #[tokio::test(start_paused = true)]
    async fn a_pause_nobody_came_back_to_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::new(dir.path(), "127.0.0.1:11301");
        let now = now_unix();
        store.save(&store::Kept {
            watched: Vec::new(),
            paused: vec![
                store::Paused {
                    session_id: "long-gone".to_owned(),
                    paused_at: now - PAUSE_LIFETIME.as_secs() as i64 - 1,
                },
                store::Paused {
                    session_id: "still-waiting".to_owned(),
                    paused_at: now - 60,
                },
            ],
        });

        let (keepalive, _watching) = keepalive_storing(dir.path());
        keepalive.restore();
        assert_eq!(keepalive.paused_sessions(), ["still-waiting"]);
    }

    /// 畳んだ系列は置き場からも消える。
    #[tokio::test(start_paused = true)]
    async fn what_was_forgotten_does_not_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let (before, _) = keepalive_storing(dir.path());
        before.armed_by_request(series(), bound(), HORIZON, now_unix_ms());
        before.forget(&series());
        drop(before);

        let (after, _watching) = keepalive_storing(dir.path());
        after.restore();
        assert_eq!(after.armed(), 0);
    }
}
