//! 枠の利用率の推移 (DR-0044)。
//!
//! [`super::QuotaStore`] が持つのは credential ごとの**最新の 1 つ**で、上書き
//! されるので推移が残らない。ここは枠 (5 時間 / 7 日の窓、枠照会 API が返す
//! モデル別の枠など) ごとに、**利用率が動くたびに 1 行**を足して残す。行には
//! 「前にその枠が動いてから今回までに、この gateway がその credential で流した
//! トークン量」をモデル別に添える。1 % 動く間にどのモデルで何トークン使ったかを
//! 行から直接読むためのもの。
//!
//! ## 1 行の種類 (`change`)
//!
//! - `start`: このプロセスがその枠を初めて見た。差を数える起点で、トークンは
//!   添えない (起動前に流した分は数えていない)
//! - `step`: 利用率が上がった。`from` が前回の値、`since` が前回動いた (または
//!   起点の) 時刻、`tokens` がその間に流したモデル別の量
//! - `rollover`: 窓が切り替わった (前回のリセット時刻を過ぎてリセット時刻が
//!   変わった) か、利用率が下がった。前の窓に流した分と新しい窓に流した分が
//!   混ざるので、差は添えずに起点を置き直す
//!
//! 値が変わらない観測は行にしない (応答のたびに来るので、書くと応答の数だけ
//! 行が並ぶ)。リセット時刻だけが動いた観測も、窓が切り替わっていなければ
//! 行にしない。
//!
//! ## 数えられないもの
//!
//! トークンはこのプロセスが流した分だけ。同じ credential を他のプロセス
//! (stable / unstable の兄弟) や gateway の外 (別のマシン、Web) で使った分は
//! 利用率には効くが、ここには現れない。兄弟の分はそれぞれが自分のファイルに
//! 書くので、読む側が期間で合わせる。
//!
//! 枠の観測は応答ヘッダが届いた時点、トークンは本文を読み終えた時点で数える。
//! だから 1 本の応答のトークンは、その 1 本のヘッダで動いた段ではなく次の段に
//! 入る (upstream がヘッダの利用率にその 1 本を含めているかは確かめていない)。
//! 区間の境目は 1 本ぶんずれるが、ずれ方は一定なので行を並べて読めば打ち消し
//! 合う。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gateway_core::daily_log::DailyLog;
use serde::{Deserialize, Serialize};

use super::{QuotaLimit, Snapshot, Window};
use crate::credential::CredentialId;
use crate::metering::TokenUsage;
use crate::stats::{Counters, Stats};

/// 置き場の下に掘るディレクトリの名前。ファイルは `<日付>.<書き手>.jsonl`。
///
/// 日次集計 (`<日付>.<書き手>.json`) と同じ階層に並べず下に分ける。日次集計は
/// 置き場を走査して日付で始まるファイルを読むので、同じ階層に並ぶと名前の
/// 形だけで取り違えの危険を抱えることになる。
pub const DIR: &str = "usage-history";

/// ファイルの 1 行。
///
/// 枠の値は観測した経路の語のまま持つ。応答ヘッダの窓は `utilization`
/// (0.0〜1.0)、枠照会 API の枠は `percent` (0〜100)。言い換えないのは、
/// 同じ「7 日」でも両者は別の物差しで値が一致しないため (DR-0007)。どちらの
/// 経路の値かは `source` が言う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// 観測した時刻 (Unix ミリ秒)。
    pub observed_at: i64,
    /// credential の名前 (経路の名前ではない)。
    pub credential: String,
    #[serde(flatten)]
    pub reading: Reading,
    #[serde(flatten)]
    pub change: Change,
}

/// どの経路で観測した、どの枠か。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Reading {
    /// 転送の応答ヘッダから読んだ窓 ([`Snapshot`] の `5h` / `7d` の欄)。
    Headers(HeaderWindow),
    /// 枠照会 API ([`crate::provider::QuotaApi`]) が返した枠。
    QuotaApi(QuotaLimit),
}

/// 応答ヘッダの窓 1 つ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderWindow {
    /// [`Snapshot`] の欄名 (`5h` / `7d`)。provider によっては欄名と周期が
    /// 一致しないので、周期は `window_seconds` で読む。
    pub window: String,
    #[serde(flatten)]
    pub state: Window,
}

/// この行が前の行に対して何を表すか。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    /// 差を数える起点。このプロセスがその枠を初めて見た。
    Start,
    /// 窓が切り替わった、または利用率が下がった。差を数える起点を置き直す。
    Rollover,
    /// 利用率が上がった。
    Step {
        /// 前の行の値 (この行の値と同じ物差し)。
        from: f64,
        /// 前の行の時刻 (Unix ミリ秒)。トークンはこの時刻から数える。
        since: i64,
        /// その間にこのプロセスがこの credential で流した量。モデル → 集計。
        /// 区分は日次集計のファイルと同じで、upstream が言った数のまま。
        tokens: BTreeMap<String, Counters>,
    },
}

/// 枠を見分ける鍵。同じ credential でも経路 (ヘッダ / 枠照会) と枠ごとに
/// 別の系列として数える。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SeriesKey {
    credential: String,
    source: &'static str,
    name: String,
    model: Option<String>,
    model_id: Option<String>,
}

/// 系列ごとに覚えておく、前に行を書いた時点。
struct Last {
    value: f64,
    /// 最後に見たリセット時刻。値が動かなくても更新する。
    reset: Option<i64>,
    observed_at: i64,
    /// 前に行を書いた時点の、起動してからの累計 ([`Stats::running_totals`])。
    totals: BTreeMap<String, Counters>,
}

impl Last {
    /// 窓が切り替わったか、利用率が下がったか。
    ///
    /// 切り替わりは「前に見たリセット時刻を過ぎた観測で、リセット時刻が
    /// 変わった」で判定する。リセット時刻が変わっただけでは判定しない —
    /// 切り替わっていない窓のリセット時刻が観測ごとに揺れると、そのたびに
    /// 起点が置き直されて差が数えられなくなる。過ぎたリセット時刻をそのまま
    /// 言い続ける観測も切り替わりとは見ない (毎回起点を置き直すことになる)。
    fn rolled_over(&self, value: f64, reset: Option<i64>, at: i64) -> bool {
        let passed = self.reset.is_some_and(|r| at >= r && reset != Some(r));
        passed || value < self.value
    }
}

/// 行を書く判断。
enum Decision {
    Start,
    Rollover,
    Step { from: f64, since: i64 },
}

/// 枠の利用率の推移を貯める器。
pub struct UsageHistory {
    log: DailyLog,
    stats: Arc<Stats>,
    series: Mutex<BTreeMap<SeriesKey, Last>>,
}

impl UsageHistory {
    /// 置き場と書き手の名前を決めて作る。トークンは `stats` が積んだ分を読む。
    ///
    /// 読み戻しは無い。系列の起点はプロセスごとに `start` から始める — 前の
    /// プロセスが最後に書いた値を起点にすると、停止中に流れた (このプロセスが
    /// 数えていない) 分の利用率の動きが、起動後のトークンに付いてしまう。
    pub fn new(dir: impl Into<PathBuf>, writer: &str, stats: Arc<Stats>) -> Self {
        Self {
            log: DailyLog::new(dir, writer),
            stats,
            series: Mutex::new(BTreeMap::new()),
        }
    }

    /// 応答ヘッダから読んだスナップショットの窓を、系列ごとに見る。
    ///
    /// 使用率が読めなかった窓は飛ばす (動いたかどうかが言えない)。
    pub fn observe_snapshot(&self, id: &CredentialId, snapshot: &Snapshot) {
        for (name, window) in [("5h", &snapshot.five_hour), ("7d", &snapshot.seven_day)] {
            let Some(window) = window else {
                continue;
            };
            let Some(value) = window.utilization else {
                continue;
            };
            let key = SeriesKey {
                credential: id.to_string(),
                source: "headers",
                name: name.to_owned(),
                model: None,
                model_id: None,
            };
            let reading = Reading::Headers(HeaderWindow {
                window: name.to_owned(),
                state: window.clone(),
            });
            self.observe(key, value, window.reset, snapshot.observed_at, reading);
        }
    }

    /// 枠照会 API が返した枠を、系列ごとに見る。`observed_at_ms` は答えを
    /// 受け取った時刻 (Unix ミリ秒)。
    pub fn observe_limits(&self, id: &CredentialId, limits: &[QuotaLimit], observed_at_ms: i64) {
        for limit in limits {
            let key = SeriesKey {
                credential: id.to_string(),
                source: "quota_api",
                name: limit.kind.clone(),
                model: limit.model.clone(),
                model_id: limit.model_id.clone(),
            };
            let reading = Reading::QuotaApi(limit.clone());
            self.observe(key, limit.percent, limit.resets_at, observed_at_ms, reading);
        }
    }

    fn observe(&self, key: SeriesKey, value: f64, reset: Option<i64>, at: i64, reading: Reading) {
        let mut series = self.series.lock().unwrap_or_else(|e| e.into_inner());
        let decision = match series.get_mut(&key) {
            None => Decision::Start,
            Some(last) if last.rolled_over(value, reset, at) => Decision::Rollover,
            Some(last) if value > last.value => Decision::Step {
                from: last.value,
                since: last.observed_at,
            },
            Some(last) => {
                // 動いていない。次の切り替わりの判定に使うので、リセット時刻だけ
                // 新しくしておく。起点 (値・時刻・累計) は動かさない。
                last.reset = reset;
                return;
            }
        };

        let totals = self.stats.running_totals(&key.credential);
        let change = match decision {
            Decision::Start => Change::Start,
            Decision::Rollover => Change::Rollover,
            Decision::Step { from, since } => {
                let before = series.get(&key).map(|last| &last.totals);
                Change::Step {
                    from,
                    since,
                    tokens: difference(&totals, before),
                }
            }
        };
        self.log.push(
            at.div_euclid(1000),
            &Entry {
                observed_at: at,
                credential: key.credential.clone(),
                reading,
                change,
            },
        );
        series.insert(
            key,
            Last {
                value,
                reset,
                observed_at: at,
                totals,
            },
        );
    }

    /// 溜めた行をディスクへ落とす (best-effort。落とし損ねた行は次の周回で書く)。
    pub fn flush(&self) -> std::io::Result<()> {
        self.log.flush()
    }
}

/// 2 時点の累計の差。どの区分も 0 なら、そのモデルは欄ごと出さない。
///
/// 累計は減らない (起動してから足すだけ) ので、差が負になることは無い。念のため
/// 0 で止める。
fn difference(
    now: &BTreeMap<String, Counters>,
    before: Option<&BTreeMap<String, Counters>>,
) -> BTreeMap<String, Counters> {
    let mut out = BTreeMap::new();
    for (model, counters) in now {
        let earlier = before.and_then(|before| before.get(model));
        let requests = counters
            .requests
            .saturating_sub(earlier.map_or(0, |e| e.requests));
        let mut tokens = TokenUsage::default();
        for (kind, count) in &counters.tokens.tokens {
            let was = earlier.and_then(|e| e.tokens.get(kind)).unwrap_or(0);
            let grown = count.saturating_sub(was);
            if grown > 0 {
                tokens.set(kind.clone(), grown);
            }
        }
        if requests > 0 || !tokens.is_empty() {
            out.insert(model.clone(), Counters { requests, tokens });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::TokenKind;
    use std::path::Path;

    /// 2026-07-29T12:00:00Z を Unix ミリ秒で。
    const NOW: i64 = 1_785_326_400_000;
    const MINUTE: i64 = 60_000;
    const FIVE_HOURS: i64 = 5 * 60 * MINUTE;
    /// 窓のリセット時刻 (NOW から 2 時間後)。
    const RESET: i64 = NOW + 2 * 60 * MINUTE;

    fn rig(dir: &Path) -> (UsageHistory, Arc<Stats>) {
        let stats = Arc::new(Stats::new(dir.join("stats"), "8402"));
        (
            UsageHistory::new(dir.join("history"), "8402", Arc::clone(&stats)),
            stats,
        )
    }

    /// 落とした行を全部、ファイル名 (日付) の順・行の順に読む。
    fn entries(dir: &Path) -> Vec<Entry> {
        let mut paths: Vec<_> = std::fs::read_dir(dir.join("history"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        paths.sort();
        paths
            .iter()
            .flat_map(|path| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<Entry>>()
            })
            .collect()
    }

    fn tokens(input: u64, cache_read: u64, output: u64) -> TokenUsage {
        let mut usage = TokenUsage::default();
        usage.set(TokenKind::input(), input);
        usage.set(TokenKind::input_cache_read(), cache_read);
        usage.set(TokenKind::output(), output);
        usage
    }

    /// 応答ヘッダの 5 時間の窓だけを持つスナップショット。
    fn five_hour(at: i64, utilization: f64, reset: i64) -> Snapshot {
        Snapshot::new(
            at,
            Some(
                Window {
                    utilization: Some(utilization),
                    status: Some("allowed".to_owned()),
                    ..Window::default()
                }
                .with_reset(Some(reset))
                .with_window_seconds(Some(5 * 60 * 60)),
            ),
            None,
            None,
        )
        .unwrap()
    }

    fn step_of(entry: &Entry) -> (f64, i64, &BTreeMap<String, Counters>) {
        match &entry.change {
            Change::Step {
                from,
                since,
                tokens,
            } => (*from, *since, tokens),
            other => panic!("not a step: {other:?}"),
        }
    }

    fn count(counters: &Counters, kind: TokenKind) -> u64 {
        counters.tokens.get(&kind).unwrap_or(0)
    }

    // ---------- 1 行の種類 ----------

    /// 初めて見た枠は起点の行になり、トークンは添えない。
    ///
    /// 起動前に流した分はこのプロセスが数えていないので、最初の値までの
    /// 動きにトークンを対応づけられない。起動前の累計を添えると、起点の行が
    /// 「その量で今の利用率になった」と読めてしまう。
    #[test]
    fn the_first_reading_is_a_start_without_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(10, 0, 1));

        history.observe_snapshot(&CredentialId::new("a"), &five_hour(NOW, 0.3, RESET));
        history.flush().unwrap();

        let lines = entries(dir.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].change, Change::Start);
        assert_eq!(lines[0].credential, "a");
        assert_eq!(lines[0].observed_at, NOW);
        let Reading::Headers(window) = &lines[0].reading else {
            panic!("read from the headers");
        };
        assert_eq!(window.window, "5h");
        assert_eq!(window.state.utilization, Some(0.3));
        assert_eq!(window.state.reset, Some(RESET));
    }

    /// 利用率が上がると、前に動いてから今までに流した量をモデル別に添える。
    ///
    /// 前の行より前に流した分は入れない (それは前の区間の分)。区分は日次集計と
    /// 同じ名前のまま (input / input.cache_read / output …) で、本数も添える。
    #[test]
    fn a_rise_carries_the_tokens_since_the_previous_line() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");

        history.observe_snapshot(&id, &five_hour(NOW, 0.30, RESET));
        stats.record(NOW / 1000, Some("a"), "big", "main", &tokens(10, 1000, 5));
        stats.record(
            NOW / 1000,
            Some("a"),
            "big",
            "keepalive",
            &tokens(1, 500, 1),
        );
        stats.record(NOW / 1000, Some("a"), "small", "main", &tokens(3, 0, 3));
        history.observe_snapshot(&id, &five_hour(NOW + MINUTE, 0.31, RESET));
        stats.record(NOW / 1000, Some("a"), "big", "main", &tokens(7, 0, 7));
        history.observe_snapshot(&id, &five_hour(NOW + 2 * MINUTE, 0.33, RESET));
        history.flush().unwrap();

        let lines = entries(dir.path());
        assert_eq!(lines.len(), 3);

        let (from, since, tokens) = step_of(&lines[1]);
        assert_eq!(from, 0.30);
        assert_eq!(since, NOW, "counted from the start line");
        assert_eq!(
            tokens["big"].requests, 2,
            "every origin spends the same window"
        );
        assert_eq!(count(&tokens["big"], TokenKind::input()), 11);
        assert_eq!(count(&tokens["big"], TokenKind::input_cache_read()), 1500);
        assert_eq!(count(&tokens["small"], TokenKind::output()), 3);

        let (from, since, tokens) = step_of(&lines[2]);
        assert_eq!(from, 0.31, "a jump of two points is one line");
        assert_eq!(since, NOW + MINUTE);
        assert_eq!(tokens.len(), 1, "a model idle in this span is omitted");
        assert_eq!(
            tokens["big"].requests, 1,
            "only what came after the last line"
        );
        assert_eq!(count(&tokens["big"], TokenKind::input()), 7);
    }

    /// 値が変わらない観測は行にしない。応答のたびに来るので、書くと応答の
    /// 数だけ行が並ぶ。その間に流した分は、次に動いた行へまとめて入る。
    #[test]
    fn an_unchanged_value_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");

        history.observe_snapshot(&id, &five_hour(NOW, 0.30, RESET));
        for i in 1..=3 {
            stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(1, 0, 0));
            history.observe_snapshot(&id, &five_hour(NOW + i * MINUTE, 0.30, RESET));
        }
        history.observe_snapshot(&id, &five_hour(NOW + 4 * MINUTE, 0.31, RESET));
        history.flush().unwrap();

        let lines = entries(dir.path());
        assert_eq!(lines.len(), 2);
        let (_, since, tokens) = step_of(&lines[1]);
        assert_eq!(
            since, NOW,
            "the start stays the baseline while nothing moves"
        );
        assert_eq!(tokens["m"].requests, 3);
    }

    /// 他の credential で流した分は添えない。枠は credential ごとに別。
    #[test]
    fn tokens_of_another_credential_are_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");

        history.observe_snapshot(&id, &five_hour(NOW, 0.30, RESET));
        stats.record(NOW / 1000, Some("b"), "m", "main", &tokens(100, 0, 100));
        history.observe_snapshot(&id, &five_hour(NOW + MINUTE, 0.31, RESET));
        history.flush().unwrap();

        let lines = entries(dir.path());
        let (_, _, tokens) = step_of(&lines[1]);
        assert!(tokens.is_empty(), "{tokens:?}");
    }

    // ---------- 窓の切り替わり ----------

    /// 前のリセット時刻を過ぎて窓が切り替わったら、差を添えずに起点を置き直す。
    ///
    /// 切り替わりをまたぐ区間には、前の窓で使った分 (最後の値は見えていない) と
    /// 新しい窓で使った分が混ざるので、1 % あたりの量として読めない。次に
    /// 動いた行は、切り替わりの行から数える。
    #[test]
    fn a_window_rollover_restarts_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");
        let next_reset = RESET + FIVE_HOURS;

        history.observe_snapshot(&id, &five_hour(NOW, 0.80, RESET));
        stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(50, 0, 0));
        // 新しい窓では同じ値でも、切り替わったことは行に残す。
        history.observe_snapshot(&id, &five_hour(RESET + MINUTE, 0.80, next_reset));
        stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(2, 0, 0));
        history.observe_snapshot(&id, &five_hour(RESET + 2 * MINUTE, 0.81, next_reset));
        history.flush().unwrap();

        let lines = entries(dir.path());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].change, Change::Rollover);
        let (from, since, tokens) = step_of(&lines[2]);
        assert_eq!(from, 0.80);
        assert_eq!(since, RESET + MINUTE, "counted from the rollover");
        assert_eq!(count(&tokens["m"], TokenKind::input()), 2);
    }

    /// 利用率が下がった観測も、起点の置き直しにする。下がった区間のトークンは
    /// どの 1 % にも対応づけられない。
    #[test]
    fn a_fall_restarts_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let (history, _stats) = rig(dir.path());
        let id = CredentialId::new("a");

        history.observe_snapshot(&id, &five_hour(NOW, 0.50, RESET));
        history.observe_snapshot(&id, &five_hour(NOW + MINUTE, 0.10, RESET));
        history.flush().unwrap();

        assert_eq!(entries(dir.path())[1].change, Change::Rollover);
    }

    /// 窓が切り替わっていない間にリセット時刻だけ揺れても、行にしない。
    ///
    /// リセット時刻の変化を切り替わりと見ると、揺れるたびに起点が置き直されて
    /// 差が数えられなくなる。切り替わりは「前のリセット時刻を過ぎたか」で見る。
    /// 揺れた後の時刻は覚えておき、次の切り替わりの判定に使う。
    #[test]
    fn a_jittering_reset_is_not_a_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let (history, _stats) = rig(dir.path());
        let id = CredentialId::new("a");

        history.observe_snapshot(&id, &five_hour(NOW, 0.50, RESET));
        history.observe_snapshot(&id, &five_hour(NOW + MINUTE, 0.50, RESET + 1000));
        history.observe_snapshot(&id, &five_hour(NOW + 2 * MINUTE, 0.51, RESET - 1000));
        // 揺れた後の時刻 (RESET - 1000) を過ぎて、新しい窓の時刻が来た。
        history.observe_snapshot(&id, &five_hour(RESET, 0.51, RESET + FIVE_HOURS));
        history.flush().unwrap();

        let changes: Vec<_> = entries(dir.path()).into_iter().map(|e| e.change).collect();
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert!(matches!(changes[1], Change::Step { .. }));
        assert_eq!(changes[2], Change::Rollover);
    }

    /// 過ぎたリセット時刻をそのまま言い続ける観測は、切り替わりと見ない。
    /// 毎回切り替わりにすると、応答のたびに行が並び差も数えられない。
    #[test]
    fn a_stale_reset_repeated_is_not_a_rollover_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let (history, _stats) = rig(dir.path());
        let id = CredentialId::new("a");

        history.observe_snapshot(&id, &five_hour(NOW, 0.50, RESET));
        history.observe_snapshot(&id, &five_hour(RESET + MINUTE, 0.50, RESET));
        history.observe_snapshot(&id, &five_hour(RESET + 2 * MINUTE, 0.51, RESET));
        history.flush().unwrap();

        let changes: Vec<_> = entries(dir.path()).into_iter().map(|e| e.change).collect();
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(matches!(changes[1], Change::Step { .. }));
    }

    // ---------- 系列の分け方 ----------

    /// 5 時間と 7 日の窓は別々に数える。一方が動いても、もう一方の起点は動かない。
    ///
    /// 7 日の窓の 1 段は 5 時間の窓の何段分にも当たる。起点を共有すると、7 日の
    /// 行に添える量が「前に 5 時間の窓が動いてから」に縮んでしまう。
    #[test]
    fn each_window_keeps_its_own_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");
        let both = |at: i64, short: f64, long: f64| {
            let mut snapshot = five_hour(at, short, RESET);
            snapshot.seven_day = Some(
                Window {
                    utilization: Some(long),
                    ..Window::default()
                }
                .with_reset(Some(NOW + 7 * 24 * 60 * MINUTE))
                .with_window_seconds(Some(7 * 24 * 60 * 60)),
            );
            snapshot
        };

        history.observe_snapshot(&id, &both(NOW, 0.10, 0.40));
        stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(1, 0, 0));
        history.observe_snapshot(&id, &both(NOW + MINUTE, 0.11, 0.40));
        stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(2, 0, 0));
        history.observe_snapshot(&id, &both(NOW + 2 * MINUTE, 0.12, 0.41));
        history.flush().unwrap();

        let lines = entries(dir.path());
        let weekly: Vec<_> = lines
            .iter()
            .filter(|e| matches!(&e.reading, Reading::Headers(w) if w.window == "7d"))
            .collect();
        assert_eq!(weekly.len(), 2, "a start and one step");
        let (from, since, tokens) = step_of(weekly[1]);
        assert_eq!(from, 0.40);
        assert_eq!(since, NOW, "not reset by the 5h window moving");
        assert_eq!(count(&tokens["m"], TokenKind::input()), 3);
        assert_eq!(lines.len(), 5, "5h: start + 2 steps, 7d: start + 1 step");
    }

    /// 使用率が読めなかった窓 (status だけの窓など) は、動いたかが言えないので
    /// 行にしない。
    #[test]
    fn a_window_without_utilization_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (history, _stats) = rig(dir.path());
        let snapshot = Snapshot::new(
            NOW,
            Some(Window {
                status: Some("allowed".to_owned()),
                ..Window::default()
            }),
            None,
            None,
        )
        .unwrap();

        history.observe_snapshot(&CredentialId::new("a"), &snapshot);
        history.flush().unwrap();
        assert!(entries(dir.path()).is_empty());
    }

    /// 枠照会 API の枠は、種類とモデルごとに別の系列にする。値は `percent` の
    /// まま持つ。
    ///
    /// 週の全体の枠とモデル別の週の枠は同じ `weekly_*` でも別の枠で、モデル別の
    /// 枠は応答ヘッダに出ない。モデル別の枠の 1 段にどのモデルの量が対応するかを
    /// 読むには、モデル別の枠を独立に数える必要がある。
    #[test]
    fn queried_limits_are_kept_per_kind_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");
        let limit = |kind: &str, model: Option<&str>, percent: f64| QuotaLimit {
            kind: kind.to_owned(),
            percent,
            severity: Some("normal".to_owned()),
            resets_at: Some(RESET),
            model: model.map(str::to_owned),
            model_id: None,
            window_seconds: Some(7 * 24 * 60 * 60),
            is_active: false,
        };

        history.observe_limits(
            &id,
            &[
                limit("weekly_all", None, 40.0),
                limit("weekly_scoped", Some("Big"), 20.0),
            ],
            NOW,
        );
        stats.record(NOW / 1000, Some("a"), "big-1", "main", &tokens(9, 0, 9));
        history.observe_limits(
            &id,
            &[
                limit("weekly_all", None, 40.0),
                limit("weekly_scoped", Some("Big"), 21.0),
            ],
            NOW + MINUTE,
        );
        history.flush().unwrap();

        let lines = entries(dir.path());
        assert_eq!(
            lines.len(),
            3,
            "two starts, then only the scoped limit moved"
        );
        let Reading::QuotaApi(moved) = &lines[2].reading else {
            panic!("from the quota API");
        };
        assert_eq!(moved.kind, "weekly_scoped");
        assert_eq!(moved.model.as_deref(), Some("Big"));
        assert_eq!(moved.percent, 21.0);
        let (from, _, tokens) = step_of(&lines[2]);
        assert_eq!(from, 20.0, "the same yardstick as the value");
        assert_eq!(tokens["big-1"].requests, 1);
    }

    /// 応答ヘッダの窓と枠照会 API の枠は、同じ 7 日でも別の系列。片方の観測で
    /// もう片方の起点を動かさない (両者は同時刻に違う値を返す)。
    #[test]
    fn headers_and_the_quota_api_are_separate_series() {
        let dir = tempfile::tempdir().unwrap();
        let (history, _stats) = rig(dir.path());
        let id = CredentialId::new("a");
        let limit = QuotaLimit {
            kind: "5h".to_owned(),
            percent: 30.0,
            severity: None,
            resets_at: Some(RESET),
            model: None,
            model_id: None,
            window_seconds: Some(5 * 60 * 60),
            is_active: true,
        };

        history.observe_snapshot(&id, &five_hour(NOW, 0.30, RESET));
        history.observe_limits(&id, std::slice::from_ref(&limit), NOW + MINUTE);
        history.flush().unwrap();

        let changes: Vec<_> = entries(dir.path()).into_iter().map(|e| e.change).collect();
        assert_eq!(changes, vec![Change::Start, Change::Start]);
    }

    // ---------- ファイルの形 ----------

    /// 1 行の JSON の形。読む側 (分析の手元のスクリプト) はこの欄名で読む。
    ///
    /// 時刻は Unix ミリ秒の数、窓の欄は利用状況 (`/llm-gateway/usage`) の
    /// スナップショットと同じ名前、トークンは日次集計と同じ `requests` /
    /// `tokens` の形。
    #[test]
    fn the_line_shape() {
        let dir = tempfile::tempdir().unwrap();
        let (history, stats) = rig(dir.path());
        let id = CredentialId::new("a");
        history.observe_snapshot(&id, &five_hour(NOW, 0.30, RESET));
        stats.record(NOW / 1000, Some("a"), "m", "main", &tokens(1, 2, 3));
        history.observe_snapshot(&id, &five_hour(NOW + MINUTE, 0.31, RESET));
        history.flush().unwrap();

        let date = crate::credential::time::local_date(NOW / 1000);
        let raw =
            std::fs::read_to_string(dir.path().join(format!("history/{date}.8402.jsonl"))).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(
            lines[0],
            serde_json::json!({
                "observed_at": NOW, "credential": "a", "change": "start",
                "source": "headers", "window": "5h", "utilization": 0.30,
                "status": "allowed", "reset": RESET, "window_seconds": 18000,
            })
        );
        assert_eq!(
            lines[1],
            serde_json::json!({
                "observed_at": NOW + MINUTE, "credential": "a", "change": "step",
                "source": "headers", "window": "5h", "utilization": 0.31,
                "status": "allowed", "reset": RESET, "window_seconds": 18000,
                "from": 0.30, "since": NOW,
                "tokens": {"m": {"requests": 1, "tokens": {
                    "input": 1, "input.cache_read": 2, "output": 3}}},
            })
        );
    }
}
