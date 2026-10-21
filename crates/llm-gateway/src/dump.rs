//! request と response の本文を、時限と容量の上限つきで一時的に書き出す (DR-0038)。
//!
//! 一般形は claude-rules-personal の reference `proxy-dump-design` が正本で、ここは
//! それを llm-gateway に当てはめたもの。有効化は制御口からだけで、`start` 1 回が
//! 1 つの系列 (`dump_id`、UUIDv7) になる。系列の表はメモリにだけ置き、restart で
//! 消える。
//!
//! 置き場は `[stats] dir` の `dumps/<unit>/<ns>/<dump_id>/` で、中身は進行の
//! イベントログ `index.jsonl` と、1 本ごとの `<ts>-<seq>.request.json` /
//! `.request.body`、upstream の試行ごとの `<ts>-<seq>.<attempt>.response.json` /
//! `.response.body`。ディレクトリは 700、ファイルは 600。
//!
//! **本体を阻害しない** (reference `model`): 中継の経路でやるのは、動いている系列が
//! あるかの atomic の load と、容量の atomic な先加算と、書き手のスレッドへの
//! 上限つきの受け渡しだけ。ディスクへの書き込みは系列ごとの書き手のスレッド
//! ([`writer`]) がする。書き手が追いつけない・書けない時はその系列を `error` で
//! 止め、中継は何事も無く続ける。
//!
//! 片付けは unit の起動時と `start` の時だけで、`dump_id` の時刻部 (= 始めた時刻) に
//! `cleanup_ttl` を足した `delete_after` を過ぎた系列をディレクトリごと消す。

mod capture;
mod record;
mod writer;

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

pub use capture::{Capture, RequestBody, RequestHead, Settling, Upstream};

/// fs の空き容量を測り直す間隔 (DR-0038 未決の裁定)。
const FS_CHECK_MS: i64 = 10_000;

/// 系列が止まった理由 (reference `model` の表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Timeout,
    MaxBytes,
    TotalLimit,
    FsMinFree,
    Stop,
    Disconnected,
    Lagged,
    Error,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::MaxBytes => "max_bytes",
            Self::TotalLimit => "total_limit",
            Self::FsMinFree => "fs_min_free",
            Self::Stop => "stop",
            Self::Disconnected => "disconnected",
            Self::Lagged => "lagged",
            Self::Error => "error",
        }
    }
}

/// 動いている系列が共有する、全体の実使用量と fs の空き容量。
struct Usage {
    /// 容量の数。系列ごとの書いた量・止まったかと合わせて、この錠の中だけで
    /// 読み書きする。
    ///
    /// Design rationale: 系列の量・全体の量・fs の見込みは 3 つの上限を同時に
    /// 満たすかで決まり、止める側は系列の量を全体から引く。atomic を別々に
    /// 足し引きすると、確かめてから足すまでの間に止める側が割り込み、順序次第で
    /// 他の系列の量を消したり止まった系列の量を残したりする。区間は数の読み書き
    /// だけで I/O も alloc もしないので、中継を待たせない (動いている系列が無い
    /// 間は錠に触らない)。
    accounting: Mutex<Accounting>,
    /// 最後に測った空き容量 (答えの `fs_free_bytes`)。
    fs_measured: AtomicU64,
    /// それを測った時刻。0 は未計測。
    fs_checked_at: AtomicI64,
    /// 測る場所 (`dumps/` が無ければ、在る祖先)。
    root: PathBuf,
}

/// 容量の数。
#[derive(Debug, Default)]
struct Accounting {
    /// 動いている系列が書いた本文の合計。止まった系列の分は差し引く。
    total: u64,
    /// fs の空き容量の見込み: 測った値から、その後に書くことにした分を引いたもの。
    fs_available: u64,
    /// 書くことにしたが、書き手がまだ書いていない分。測り直す時に空き容量から引く。
    fs_pending: u64,
}

impl Usage {
    /// 空き容量を測り直す。間隔より新しければ何もしない (`force` で必ず)。
    /// 測るのは錠の外、置き直すのは錠の中。
    fn refresh(&self, now: i64, force: bool) {
        let checked = self.fs_checked_at.load(Ordering::Acquire);
        if !force && checked != 0 && now - checked < FS_CHECK_MS {
            return;
        }
        let Some(free) = fs_free(&self.root) else {
            return;
        };
        {
            let mut accounting = lock(&self.accounting);
            accounting.fs_available = free.saturating_sub(accounting.fs_pending);
        }
        self.fs_measured.store(free, Ordering::Release);
        self.fs_checked_at.store(now, Ordering::Release);
    }

    /// 書き手が書いた (書けなかった) 分を未確定から外す。
    fn written(&self, bytes: u64) {
        let mut accounting = lock(&self.accounting);
        accounting.fs_pending = accounting.fs_pending.saturating_sub(bytes);
    }

    fn total(&self) -> u64 {
        lock(&self.accounting).total
    }
}

/// `path` (無ければ在る祖先) の fs で、一般の利用者が使える空き容量。
fn fs_free(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let stat = rustix::fs::statvfs(existing).ok()?;
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// 書いてよい量と、上限に掛かったならその理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grant {
    bytes: u64,
    limited: Option<StopReason>,
}

/// 1 つの系列。
pub(crate) struct Series {
    id: String,
    unit: String,
    ns: String,
    dir: PathBuf,
    labels: BTreeMap<String, String>,
    started_at: i64,
    expires_at: i64,
    delete_after: i64,
    max_bytes: u64,
    /// 始めた時の `[dump] max_total_bytes` / `fs_min_free`。
    max_total_bytes: u64,
    fs_min_free: u64,
    /// 本文 (`*.body`) に書いた合計。メタ json と `index.jsonl` は数えない。
    bytes_written: AtomicU64,
    active: AtomicBool,
    /// 止まった理由と時刻。動いている間は `None`。
    stopped: Mutex<Option<(StopReason, i64)>>,
    writer: writer::Writer,
    counter: Arc<AtomicUsize>,
    usage: Arc<Usage>,
    /// 書き手のスレッド。`prune` が消す前に終わりを待つ。
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Series {
    /// 動いていれば止めて、理由を log に出す。既に止まっていれば何もしない。
    ///
    /// 全体の実使用量が数えるのは動いている系列だけなので、書いた量を引く
    /// (reference `capacity`)。止めるのと引くのは容量の錠の中で 1 回にする
    /// ([`Usage::accounting`])。
    pub(crate) fn stop(&self, reason: StopReason, now: i64) {
        let written = {
            let mut accounting = lock(&self.usage.accounting);
            if !self.active.swap(false, Ordering::AcqRel) {
                return;
            }
            *lock(&self.stopped) = Some((reason, now));
            let written = self.bytes_written.load(Ordering::Acquire);
            accounting.total = accounting.total.saturating_sub(written);
            written
        };
        self.counter.fetch_sub(1, Ordering::AcqRel);
        info!(
            dump_id = %self.id,
            ns = %self.ns,
            reason = reason.as_str(),
            bytes_written = written,
            "a dump stopped"
        );
        self.writer.wake();
    }

    /// 本文に書いた量。
    fn written(&self) -> u64 {
        self.bytes_written.load(Ordering::Acquire)
    }

    /// `now` に動いているか。時限を過ぎていればここで止める。
    ///
    /// 時限は timer の発火だけに頼らず、書く時点で壁時計と比べて守る。
    /// スリープで timer が遅れても、過ぎた 1 本は書かない。
    pub(crate) fn live(&self, now: i64) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        if now >= self.expires_at {
            self.stop(StopReason::Timeout, now);
            return false;
        }
        true
    }

    /// 書き手へ渡す。止まっていれば渡さない。書き手が追いつけていなければ
    /// (受け渡しの列が溢れた) 系列を `error` で止める — 中継は待たせない。
    pub(crate) fn send(&self, message: writer::Message, now: i64) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        match self.writer.try_send(message) {
            Ok(()) => true,
            Err(why) => {
                warn!(dump_id = %self.id, why, "the dump writer could not keep up; stopping the dump");
                self.stop(StopReason::Error, now);
                false
            }
        }
    }

    /// `want` バイトを書く前に、3 つの上限 (この系列の `max_bytes`、全体の
    /// `max_total_bytes`、fs の `fs_min_free`) を確かめて足す (reference
    /// `capacity`)。`whole` なら全部書けない時は 1 バイトも書かない。
    ///
    /// 止まっていないかの確認・3 つの上限・3 つへの加算を、容量の錠の中で 1 回に
    /// する ([`Usage::accounting`])。上限に**達した**ら (超えなくても) `limited` に
    /// 理由を返す。止まっていれば 0 で、理由は付けない (止めた側が持っている)。
    fn reserve(&self, want: u64, whole: bool) -> Grant {
        let mut accounting = lock(&self.usage.accounting);
        if !self.active.load(Ordering::Acquire) {
            return Grant {
                bytes: 0,
                limited: None,
            };
        }
        let written = self.bytes_written.load(Ordering::Acquire);
        let room = [
            (self.max_bytes.saturating_sub(written), StopReason::MaxBytes),
            (
                self.max_total_bytes.saturating_sub(accounting.total),
                StopReason::TotalLimit,
            ),
            (
                accounting.fs_available.saturating_sub(self.fs_min_free),
                StopReason::FsMinFree,
            ),
        ];
        // いちばん狭い上限が、書ける量と止まる理由を決める。
        let (narrowest, reason) = room
            .into_iter()
            .min_by_key(|(room, _)| *room)
            .unwrap_or((0, StopReason::MaxBytes));
        let mut granted = want.min(narrowest);
        if whole && granted < want {
            granted = 0;
        }
        self.bytes_written
            .store(written + granted, Ordering::Release);
        accounting.total += granted;
        accounting.fs_available -= granted;
        accounting.fs_pending += granted;
        // 書けない分があった、または書いて上限ちょうどに達した。
        let limited = (granted < want || narrowest == granted).then_some(reason);
        Grant {
            bytes: granted,
            limited,
        }
    }

    /// [`Self::reserve`] で足したが書き手へ渡せなかった `bytes` を、全部の計数
    /// から戻す (錠の中で 1 回に)。fs の見込みと未確定分は必ず戻す。全体の量は、
    /// 系列が動いている時だけ戻す — 止まっていれば止めた側が系列の量ごと引いて
    /// いる。
    fn cancel(&self, bytes: u64) {
        let mut accounting = lock(&self.usage.accounting);
        accounting.fs_available += bytes;
        accounting.fs_pending = accounting.fs_pending.saturating_sub(bytes);
        let written = self.bytes_written.load(Ordering::Acquire);
        self.bytes_written
            .store(written.saturating_sub(bytes), Ordering::Release);
        if self.active.load(Ordering::Acquire) {
            accounting.total = accounting.total.saturating_sub(bytes);
        }
    }

    /// 書き手のスレッドが終わるのを待つ (止まった後で呼ぶ)。
    fn join(&self) {
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }

    fn state(&self) -> (&'static str, Option<StopReason>, Option<i64>) {
        match *lock(&self.stopped) {
            Some((reason, at)) => ("stopped", Some(reason), Some(at)),
            None => ("active", None, None),
        }
    }

    fn listed(&self) -> Listed {
        let (state, reason, stopped_at) = self.state();
        Listed {
            dump_id: self.id.clone(),
            dump_dir: self.dir.display().to_string(),
            ns: self.ns.clone(),
            state,
            reason: reason.map(|r| r.as_str().to_owned()),
            labels: self.labels.clone(),
            started_at: Some(self.started_at),
            expires_at: Some(self.expires_at),
            delete_after: Some(self.delete_after),
            stopped_at,
            bytes_written: Some(self.written()),
        }
    }
}

/// 毒の入った Mutex でも中身を取る。書き手の panic で中継を止めない。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 系列の表と、置き場。
pub struct Dumps {
    /// `<[stats] dir>/dumps`。
    root: PathBuf,
    /// 動いている系列の数。受け口はここだけを見て、0 なら何もしない。
    active: Arc<AtomicUsize>,
    usage: Arc<Usage>,
    /// この unit が始めた系列の全部 (止まったものも)。restart まで覚える。
    series: Mutex<BTreeMap<String, Arc<Series>>>,
    /// 片付け (空になった段を消す) と系列のディレクトリ作りを、この unit の中で
    /// 1 つずつ通す。制御の経路だけが持ち、中継の経路は触らない。
    dirs: Mutex<()>,
}

/// `start` で頼まれたこと。
#[derive(Debug, Clone, Default)]
pub struct Start<'a> {
    pub unit: &'a str,
    pub ns: &'a str,
    /// 時限 (ミリ秒)。`None` は `[dump] default_timeout`。
    pub timeout_ms: Option<i64>,
    pub max_bytes: Option<u64>,
    /// `raw` だけを受ける (DR-0038 決定 6)。
    pub store: Option<&'a str>,
    pub labels: BTreeMap<String, String>,
    /// 接続を保ってイベントを流すなら、その流し方。
    pub follow: Option<Follow>,
}

/// follow の流し方 (reference `api`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Follow {
    /// メタ json と本文も行に同梱する (`--embed bodies`)。
    pub embed: bool,
    /// 保存するか。`false` は `embed` の時だけで、止まったらディレクトリごと消す。
    pub persist: bool,
}

/// follow の購読口。`index.jsonl` の行 (と同梱の本文) が 1 行ずつ届き、系列が
/// 止まると `stop` の行の後で閉じる。捨てると系列は `disconnected` で止まる。
pub type Following = tokio::sync::mpsc::Receiver<String>;

/// `start` の答えの、系列と適用されたパラメータの欄 (reference `api`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Started {
    pub dump_id: String,
    pub dump_dir: String,
    pub ns: String,
    pub timeout_ms: i64,
    pub max_bytes: u64,
    pub store: &'static str,
    pub labels: BTreeMap<String, String>,
    pub started_at: i64,
    pub expires_at: i64,
    pub delete_after: i64,
    pub concurrency: Concurrency,
    pub storage: Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Concurrency {
    pub limit: usize,
    pub active: usize,
}

/// その時点の容量のスナップショット。書き込みを保証するものではない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Storage {
    pub active_used_bytes: u64,
    pub active_requested_bytes: u64,
    pub total_available_bytes: u64,
    pub total_limit_bytes: u64,
    pub fs_min_free_bytes: u64,
    pub fs_free_bytes: u64,
    pub fs_checked_at: i64,
}

/// `list` の 1 件、`stop` の答え。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
    pub dump_id: String,
    pub dump_dir: String,
    pub ns: String,
    pub state: &'static str,
    pub reason: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub started_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub delete_after: Option<i64>,
    pub stopped_at: Option<i64>,
    pub bytes_written: Option<u64>,
}

/// `/llm-gateway/self` と `daemon status` に載せる 1 件。本文の置き場と labels は
/// 載せない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Running {
    pub dump_id: String,
    pub ns: String,
    pub expires_at: i64,
    pub bytes_written: u64,
}

/// 頼みを断る理由。`kind` は制御口の答えの `error.kind` になる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl Dumps {
    /// `stats_dir` (`[stats] dir`) の下の `dumps/` を置き場にする。
    pub fn new(stats_dir: &Path) -> Self {
        // 答えに載せるのは絶対パス。設定に相対で書かれていても、突き合わせる人が
        // 置き場を導出し直さずに済むよう、ここで直しておく。
        let root = std::path::absolute(stats_dir)
            .unwrap_or_else(|_| stats_dir.to_path_buf())
            .join("dumps");
        Self {
            usage: Arc::new(Usage {
                accounting: Mutex::new(Accounting::default()),
                fs_measured: AtomicU64::new(0),
                fs_checked_at: AtomicI64::new(0),
                root: root.clone(),
            }),
            root,
            active: Arc::new(AtomicUsize::new(0)),
            series: Mutex::new(BTreeMap::new()),
            dirs: Mutex::new(()),
        }
    }

    /// 系列を 1 つ始める。始める前に古い系列を片付ける。
    ///
    /// 時限の timer はこの unit が持つ (tokio の上で呼ばれた時だけ張る)。
    /// timer が遅れても、書く側が `expires_at` と壁時計を比べて守る。
    pub fn start(
        self: &Arc<Self>,
        start: &Start<'_>,
        config: &crate::config::DumpConfig,
        now: i64,
    ) -> Result<Started, Refusal> {
        if start.follow.is_some() {
            return Err(Refusal::new(
                "bad_request",
                "a dump that follows needs a connection to stream to",
            ));
        }
        self.begin(start, config, now).map(|(started, _)| started)
    }

    /// 接続を保ってイベントを流す系列を始める (`--follow`)。返す購読口から
    /// `index.jsonl` の行が届く。購読口を捨てると系列は `disconnected` で止まる。
    pub fn start_following(
        self: &Arc<Self>,
        start: &Start<'_>,
        config: &crate::config::DumpConfig,
        now: i64,
    ) -> Result<(Started, Following), Refusal> {
        let (started, following) = self.begin(start, config, now)?;
        following
            .map(|following| (started, following))
            .ok_or_else(|| Refusal::new("bad_request", "follow was not asked for"))
    }

    fn begin(
        self: &Arc<Self>,
        start: &Start<'_>,
        config: &crate::config::DumpConfig,
        now: i64,
    ) -> Result<(Started, Option<Following>), Refusal> {
        if !crate::config::is_file_name_safe(start.ns) {
            return Err(Refusal::new(
                "invalid_namespace",
                format!(
                    "namespace `{}` cannot be dumped: its name becomes a directory, so it may contain only letters, digits, `.`, `_` and `-`, and must not start with `.`",
                    start.ns
                ),
            ));
        }
        let max_timeout = duration_ms(config.max_timeout);
        let timeout_ms = start
            .timeout_ms
            .unwrap_or_else(|| duration_ms(config.default_timeout));
        if timeout_ms <= 0 || timeout_ms > max_timeout {
            return Err(Refusal::new(
                "invalid_duration",
                format!(
                    "--timeout must be longer than zero and at most {} ([dump] max_timeout)",
                    humantime::format_duration(config.max_timeout)
                ),
            ));
        }
        if start.max_bytes == Some(0) {
            return Err(Refusal::new(
                "bad_request",
                "--max-bytes must be larger than zero",
            ));
        }
        if let Some(store) = start.store
            && store != "raw"
        {
            return Err(Refusal::new(
                "bad_request",
                format!(
                    "--store `{store}` is not supported; only raw is (this gateway does not ask upstreams to compress)"
                ),
            ));
        }
        for (key, value) in &start.labels {
            check_label(key).and_then(|()| check_label(value))?;
        }
        if let Some(follow) = start.follow
            && !follow.persist
            && !follow.embed
        {
            return Err(Refusal::new(
                "bad_request",
                "--no-persist needs --embed bodies: a dump that keeps nothing and streams no bodies shows nothing",
            ));
        }
        // 同時に動かせる数の枠を先に取る (取ってから重い支度をする)。支度に
        // 失敗したら枠を返す。
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= config.max_concurrency {
                return Err(Refusal::new(
                    "too_many_dumps",
                    format!(
                        "{active} dumps are running, which is the limit ([dump] max_concurrency = {}). stop one, or wait for one to end",
                        config.max_concurrency
                    ),
                ));
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => active = now,
            }
        }
        let active = active + 1;
        let opened = self.open_series(start, config, now, timeout_ms);
        let (series, following) = match opened {
            Ok(opened) => opened,
            Err(refusal) => {
                self.active.fetch_sub(1, Ordering::AcqRel);
                return Err(refusal);
            }
        };
        let id = series.id.clone();
        let dir = series.dir.clone();
        let max_bytes = series.max_bytes;
        info!(
            dump_id = %id,
            ns = %series.ns,
            expires_at = series.expires_at,
            max_bytes,
            dir = %dir.display(),
            "a dump started"
        );
        self.after_start(&series, timeout_ms);

        let requested: u64 = lock(&self.series)
            .values()
            .filter(|s| s.active.load(Ordering::Acquire))
            .map(|s| s.max_bytes)
            .sum();
        let used = self.usage.total();
        let fs_free = self.usage.fs_measured.load(Ordering::Acquire);
        let started = Started {
            dump_id: id,
            dump_dir: dir.display().to_string(),
            ns: start.ns.to_owned(),
            timeout_ms,
            max_bytes,
            store: "raw",
            labels: start.labels.clone(),
            started_at: now,
            expires_at: series.expires_at,
            delete_after: series.delete_after,
            concurrency: Concurrency {
                limit: config.max_concurrency,
                active,
            },
            storage: Storage {
                active_used_bytes: used,
                active_requested_bytes: requested,
                total_available_bytes: config
                    .max_total_bytes
                    .min(fs_free.saturating_sub(config.fs_min_free)),
                total_limit_bytes: config.max_total_bytes,
                fs_min_free_bytes: config.fs_min_free,
                fs_free_bytes: fs_free,
                fs_checked_at: self.usage.fs_checked_at.load(Ordering::Acquire),
            },
        };
        Ok((started, following))
    }

    /// 片付け・空き容量の測り直し・置き場と書き手の支度をして、系列を表に載せる。
    fn open_series(
        &self,
        start: &Start<'_>,
        config: &crate::config::DumpConfig,
        now: i64,
        timeout_ms: i64,
    ) -> Result<(Arc<Series>, Option<Following>), Refusal> {
        let cleanup_ttl = duration_ms(config.cleanup_ttl);
        let held = lock(&self.dirs);
        self.sweep_held(now, cleanup_ttl);
        self.usage.refresh(now, true);

        let id = uuid_v7(now, &mut |buf| rand::fill(buf));
        let dir = self.root.join(start.unit).join(start.ns).join(&id);
        make_dir(&dir).map_err(|e| {
            Refusal::new(
                "dump_dir_unwritable",
                format!("could not make {}: {e}", dir.display()),
            )
        })?;
        drop(held);
        let max_bytes = start.max_bytes.unwrap_or(config.default_max_bytes);
        let (writer, receiving) = writer::Writer::channel();
        let (follow, following) = match start.follow {
            Some(options) => {
                let (tx, rx) = tokio::sync::mpsc::channel(writer::FOLLOW_QUEUE);
                (
                    Some(writer::Follow {
                        tx,
                        embed: options.embed,
                    }),
                    Some(rx),
                )
            }
            None => (None, None),
        };
        let persist = start.follow.is_none_or(|f| f.persist);
        let series = Arc::new(Series {
            id: id.clone(),
            unit: start.unit.to_owned(),
            ns: start.ns.to_owned(),
            dir: dir.clone(),
            labels: start.labels.clone(),
            started_at: now,
            expires_at: now.saturating_add(timeout_ms),
            delete_after: now.saturating_add(cleanup_ttl),
            max_bytes,
            max_total_bytes: config.max_total_bytes,
            fs_min_free: config.fs_min_free,
            bytes_written: AtomicU64::new(0),
            active: AtomicBool::new(true),
            stopped: Mutex::new(None),
            writer,
            counter: Arc::clone(&self.active),
            usage: Arc::clone(&self.usage),
            thread: Mutex::new(None),
        });
        let mut event = Map::new();
        event.insert("event".to_owned(), json!("start"));
        event.insert("ts".to_owned(), json!(now));
        event.insert("dump_id".to_owned(), json!(id));
        event.insert("unit".to_owned(), json!(start.unit));
        event.insert("ns".to_owned(), json!(start.ns));
        event.insert("timeout_ms".to_owned(), json!(timeout_ms));
        event.insert("max_bytes".to_owned(), json!(max_bytes));
        event.insert("store".to_owned(), json!("raw"));
        event.insert("labels".to_owned(), json!(start.labels));
        event.insert("started_at".to_owned(), json!(now));
        event.insert("expires_at".to_owned(), json!(series.expires_at));
        event.insert("delete_after".to_owned(), json!(series.delete_after));
        if let Some(options) = start.follow {
            event.insert("follow".to_owned(), json!(true));
            event.insert("embed".to_owned(), json!(options.embed.then_some("bodies")));
            event.insert("persist".to_owned(), json!(options.persist));
        }
        let thread = writer::spawn(Arc::clone(&series), receiving, event, follow, persist)
            .map_err(|e| {
                Refusal::new(
                    "dump_dir_unwritable",
                    format!("could not start writing {}: {e}", dir.display()),
                )
            })?;
        *lock(&series.thread) = Some(thread);
        lock(&self.series).insert(id, Arc::clone(&series));
        Ok((series, following))
    }

    /// 始めた系列の時限の timer を張る (tokio の上で呼ばれた時だけ)。
    fn after_start(&self, series: &Arc<Series>, timeout_ms: i64) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let wait = std::time::Duration::from_millis(timeout_ms.unsigned_abs());
            let expiring = Arc::downgrade(series);
            runtime.spawn(async move {
                tokio::time::sleep(wait).await;
                if let Some(series) = Weak::upgrade(&expiring) {
                    series.stop(StopReason::Timeout, crate::credential::time::now_unix_ms());
                }
            });
        }
    }

    /// 書き手が自分で系列を止めて終わるのを待つ (試験用。止めはしない)。
    #[cfg(test)]
    pub(crate) fn wait_for_writer(&self, dump_id: &str) {
        let series = lock(&self.series).get(dump_id).cloned();
        if let Some(series) = series {
            series.join();
        }
    }

    /// 書き手の居ない (列を誰も取り出さない) 系列を表に載せる (試験用)。
    #[cfg(test)]
    fn stalled(
        &self,
        ns: &str,
        config: &crate::config::DumpConfig,
        now: i64,
    ) -> (Arc<Series>, std::sync::mpsc::Receiver<writer::Message>) {
        let (writer, receiving) = writer::Writer::channel();
        let id = uuid_v7(now, &mut |buf| rand::fill(buf));
        let series = Arc::new(Series {
            id: id.clone(),
            unit: "u".to_owned(),
            ns: ns.to_owned(),
            dir: self.root.join("u").join(ns).join(&id),
            labels: BTreeMap::new(),
            started_at: now,
            expires_at: now + 60_000,
            delete_after: now + 60_000,
            max_bytes: config.default_max_bytes,
            max_total_bytes: config.max_total_bytes,
            fs_min_free: config.fs_min_free,
            bytes_written: AtomicU64::new(0),
            active: AtomicBool::new(true),
            stopped: Mutex::new(None),
            writer,
            counter: Arc::clone(&self.active),
            usage: Arc::clone(&self.usage),
            thread: Mutex::new(None),
        });
        self.active.fetch_add(1, Ordering::AcqRel);
        lock(&self.series).insert(id, Arc::clone(&series));
        (series, receiving)
    }

    /// follow の接続が閉じた。系列を `disconnected` で止める。
    pub fn disconnected(&self, dump_id: &str, now: i64) {
        let series = lock(&self.series).get(dump_id).cloned();
        if let Some(series) = series {
            series.stop(StopReason::Disconnected, now);
        }
    }

    /// 系列を時限を待たずに止める。冪等で、止まっていた系列にはその時の状態
    /// (元の `reason`) を返す。この unit の置き場に残っているだけの系列 (restart
    /// 前の分) も、止まっている系列として答える。
    ///
    /// 答える前に書き手が `stop` の行を書き終えるのを待つ (答えを受けた時点で
    /// ファイルが揃っている)。
    pub fn stop(&self, unit: &str, dump_id: &str, now: i64) -> Result<Listed, Refusal> {
        let known = lock(&self.series)
            .get(dump_id)
            .filter(|s| s.unit == unit)
            .cloned();
        if let Some(series) = known {
            series.stop(StopReason::Stop, now);
            series.join();
            return Ok(series.listed());
        }
        self.on_disk(unit)
            .into_iter()
            .find(|listed| listed.dump_id == dump_id)
            .ok_or_else(|| {
                Refusal::new(
                    "unknown_dump",
                    format!("this unit has no dump `{dump_id}`. `dump list` shows them"),
                )
            })
    }

    /// この unit の系列の一覧。止まったもの・restart 前の分も含め、選択子に
    /// 当たるものだけ。
    pub fn list(&self, unit: &str, selectors: &[String], now: i64) -> Result<Vec<Listed>, Refusal> {
        let selectors = parse_selectors(selectors)?;
        Ok(self
            .everything(unit, now)
            .into_iter()
            .filter(|listed| selectors.iter().all(|s| s.matches(listed)))
            .collect())
    }

    /// 指した系列をディレクトリごと消す。動いていれば `stop` で止めてから。
    /// `dump_ids` か選択子のどちらかが要る (無指定の全消しはしない)。
    pub fn prune(
        &self,
        unit: &str,
        dump_ids: &[String],
        selectors: &[String],
        now: i64,
    ) -> Result<Vec<String>, Refusal> {
        if dump_ids.is_empty() && selectors.is_empty() {
            return Err(Refusal::new(
                "bad_request",
                "say which dumps to prune: --dump-id <id> or a selector such as owner=kawaz",
            ));
        }
        let selectors = parse_selectors(selectors)?;
        let chosen: Vec<Listed> = self
            .everything(unit, now)
            .into_iter()
            .filter(|listed| {
                (dump_ids.is_empty() || dump_ids.contains(&listed.dump_id))
                    && selectors.iter().all(|s| s.matches(listed))
            })
            .collect();
        let mut pruned = Vec::new();
        for listed in chosen {
            let series = lock(&self.series).remove(&listed.dump_id);
            if let Some(series) = series {
                series.stop(StopReason::Stop, now);
                // 書き手が stop の行を書き終えてから消す。
                series.join();
            }
            match std::fs::remove_dir_all(&listed.dump_dir) {
                Ok(()) => pruned.push(listed.dump_id),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    pruned.push(listed.dump_id);
                }
                Err(e) => {
                    warn!(%e, dir = %listed.dump_dir, "could not prune a dump");
                }
            }
            if let Some(ns_dir) = Path::new(&listed.dump_dir).parent() {
                let _held = lock(&self.dirs);
                let _ = std::fs::remove_dir(ns_dir);
            }
        }
        Ok(pruned)
    }

    /// `/llm-gateway/self` に載せる、動いている系列。
    pub fn running(&self, now: i64) -> Vec<Running> {
        self.live(now)
            .iter()
            .map(|series| Running {
                dump_id: series.id.clone(),
                ns: series.ns.clone(),
                expires_at: series.expires_at,
                bytes_written: series.written(),
            })
            .collect()
    }

    fn live(&self, now: i64) -> Vec<Arc<Series>> {
        if self.active.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        // 時限を過ぎた系列を止める処理 (log と書き手への合図) は錠の外でする。
        let all: Vec<Arc<Series>> = lock(&self.series).values().cloned().collect();
        all.into_iter().filter(|series| series.live(now)).collect()
    }

    /// メモリにある系列と、置き場に残っているだけの系列 (restart 前の分)。
    fn everything(&self, unit: &str, now: i64) -> Vec<Listed> {
        // 時限を過ぎた系列は、ここで止めてから並べる。
        let _ = self.live(now);
        let mut listed: Vec<Listed> = lock(&self.series)
            .values()
            .filter(|s| s.unit == unit)
            .map(|s| s.listed())
            .collect();
        for found in self.on_disk(unit) {
            if !listed.iter().any(|l| l.dump_id == found.dump_id) {
                listed.push(found);
            }
        }
        listed.sort_by(|a, b| a.dump_id.cmp(&b.dump_id));
        listed
    }

    /// この unit の置き場にあって、メモリに無い系列 (restart 前の分)。
    ///
    /// labels と時刻は `index.jsonl` の `start` の行から、止まった理由は `stop` の
    /// 行から読む。`stop` の行が無いのは止める前に unit が落ちた系列で、
    /// `reason: "interrupted"`。
    fn on_disk(&self, unit: &str) -> Vec<Listed> {
        let known: Vec<String> = lock(&self.series).keys().cloned().collect();
        let mut found = Vec::new();
        for ns in subdirectories(&self.root.join(unit)) {
            for dir in subdirectories(&ns) {
                let Some(id) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                    continue;
                };
                if known.contains(&id) {
                    continue;
                }
                found.push(read_listed(&id, &ns, &dir));
            }
        }
        found
    }

    /// `delete_after` を過ぎた系列をディレクトリごと消す。消した数を返す。
    ///
    /// `delete_after` は系列ごとの `index.jsonl` の `start` の行から読む — 兄弟の
    /// unit は `[dump] cleanup_ttl` が違いうるので、呼んだ unit の値を他の unit の
    /// 系列に当てない。読めない系列 (`start` の行が無い・壊れている) だけは、
    /// `dump_id` の時刻部に呼んだ unit の `cleanup_ttl_ms` を足して代わりにする。
    ///
    /// 掃くのは `dumps/` の下全体 (他の unit の分も)。止まったままの unit は
    /// 自分の分を掃けないので、どれかの unit が上がれば全部が片付く。
    /// `max_timeout` ≤ `cleanup_ttl` なので、動いている系列を消すことはない。
    /// 空になった `<ns>/` と `<unit>/` も消す。
    pub fn sweep(&self, now: i64, cleanup_ttl_ms: i64) -> usize {
        let _held = lock(&self.dirs);
        self.sweep_held(now, cleanup_ttl_ms)
    }

    /// [`Self::sweep`] の中身 (`dirs` の錠を持って呼ぶ)。
    fn sweep_held(&self, now: i64, cleanup_ttl_ms: i64) -> usize {
        let mut removed = 0;
        for unit in subdirectories(&self.root) {
            for ns in subdirectories(&unit) {
                for series in subdirectories(&ns) {
                    let Some(started) = series
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(uuid_v7_ms)
                    else {
                        continue;
                    };
                    let delete_after = start_line(&series)
                        .and_then(|start| start.get("delete_after").and_then(Value::as_i64))
                        .unwrap_or_else(|| {
                            info!(
                                dir = %series.display(),
                                "a dump has no readable start line; judging its age by this unit's cleanup_ttl"
                            );
                            started.saturating_add(cleanup_ttl_ms)
                        });
                    if now < delete_after {
                        continue;
                    }
                    match std::fs::remove_dir_all(&series) {
                        Ok(()) => removed += 1,
                        Err(e) => {
                            warn!(%e, dir = %series.display(), "could not remove an old dump")
                        }
                    }
                }
                // 空でなければ失敗するだけ。中身のあるものは消さない。
                let _ = std::fs::remove_dir(&ns);
            }
            let _ = std::fs::remove_dir(&unit);
        }
        if removed > 0 {
            info!(removed, root = %self.root.display(), "removed dumps past their delete_after");
        }
        removed
    }

    /// ヘッダを受けて ns 認証を通した 1 本を、その ns で動いている系列の全部に
    /// 書き始める (`request_headers`)。本文は後で [`Capture::body`] が書く。
    ///
    /// 動いている系列が無ければ atomic の load 1 つで `None` (`head` も組み立て
    /// ない)。どの系列に書くかはここで決まり、途中から始めた系列にこの 1 本は
    /// 入らない。系列の表の錠は候補を写す間だけ持ち、止める処理 (log と書き手への
    /// 合図) は錠の外でする。
    pub fn capture_head(&self, head: impl FnOnce() -> RequestHead) -> Option<Arc<Capture>> {
        if self.active.load(Ordering::Acquire) == 0 {
            return None;
        }
        let head = head();
        let now = crate::credential::time::now_unix_ms();
        let candidates: Vec<Arc<Series>> = lock(&self.series)
            .values()
            .filter(|series| series.ns == head.ns && series.active.load(Ordering::Acquire))
            .cloned()
            .collect();
        let targets: Vec<Arc<Series>> = candidates
            .into_iter()
            .filter(|series| series.live(now))
            .collect();
        Capture::begin(head, targets, now)
    }
}

/// `index.jsonl` から、restart 前の系列の 1 件を組み立てる。
fn read_listed(id: &str, ns_dir: &Path, dir: &Path) -> Listed {
    let ns = ns_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut listed = Listed {
        dump_id: id.to_owned(),
        dump_dir: dir.display().to_string(),
        ns,
        state: "stopped",
        reason: Some("interrupted".to_owned()),
        labels: BTreeMap::new(),
        started_at: uuid_v7_ms(id),
        expires_at: None,
        delete_after: None,
        stopped_at: None,
        bytes_written: None,
    };
    let text = std::fs::read_to_string(dir.join("index.jsonl")).unwrap_or_default();
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("start") => {
                if let Some(labels) = event
                    .get("labels")
                    .and_then(|l| serde_json::from_value(l.clone()).ok())
                {
                    listed.labels = labels;
                }
                listed.started_at = event.get("started_at").and_then(Value::as_i64);
                listed.expires_at = event.get("expires_at").and_then(Value::as_i64);
                listed.delete_after = event.get("delete_after").and_then(Value::as_i64);
            }
            Some("stop") => {
                listed.reason = event
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                listed.stopped_at = event.get("stopped_at").and_then(Value::as_i64);
                listed.bytes_written = event.get("bytes_written").and_then(Value::as_u64);
            }
            _ => {}
        }
    }
    if listed.bytes_written.is_none() {
        listed.bytes_written = Some(body_bytes(dir));
    }
    listed
}

/// 系列の `index.jsonl` の最初の行 (`start`)。読めなければ `None`。
fn start_line(dir: &Path) -> Option<Value> {
    use std::io::BufRead as _;

    let file = std::fs::File::open(dir.join("index.jsonl")).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let line: Value = serde_json::from_str(&first).ok()?;
    (line.get("event").and_then(Value::as_str) == Some("start")).then_some(line)
}

/// 置き場にある本文 (`*.body`) の大きさの合計。
fn body_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".body"))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// labels のキーと値: 1〜63 文字、英数字と `-` `_` `.`、先頭と末尾は英数字。
fn check_label(text: &str) -> Result<(), Refusal> {
    let bytes = text.as_bytes();
    let ok = (1..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric);
    if ok {
        Ok(())
    } else {
        Err(Refusal::new(
            "bad_request",
            format!(
                "`{text}` cannot be a label key or value: write 1 to 63 letters, digits, `-`, `_` or `.`, starting and ending with a letter or digit"
            ),
        ))
    }
}

/// labels に対する選択子 (`k=v` / `k!=v` / `k`)。`state` は labels でなく系列の
/// 状態 (`active` / `stopped`) を指す。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selector {
    Equals(String, String),
    Differs(String, String),
    Has(String),
}

impl Selector {
    fn matches(&self, listed: &Listed) -> bool {
        let value = |key: &str| -> Option<String> {
            if key == "state" {
                return Some(listed.state.to_owned());
            }
            listed.labels.get(key).cloned()
        };
        match self {
            Self::Equals(key, want) => value(key).as_deref() == Some(want.as_str()),
            Self::Differs(key, want) => value(key).as_deref() != Some(want.as_str()),
            Self::Has(key) => value(key).is_some(),
        }
    }
}

fn parse_selectors(raw: &[String]) -> Result<Vec<Selector>, Refusal> {
    raw.iter()
        .map(|text| {
            let selector = if let Some((key, value)) = text.split_once("!=") {
                Selector::Differs(key.to_owned(), value.to_owned())
            } else if let Some((key, value)) = text.split_once('=') {
                Selector::Equals(key.to_owned(), value.to_owned())
            } else {
                Selector::Has(text.clone())
            };
            match &selector {
                Selector::Equals(k, v) | Selector::Differs(k, v) => {
                    check_label(k).and_then(|()| check_label(v))
                }
                Selector::Has(k) => check_label(k),
            }
            .map_err(|_| {
                Refusal::new(
                    "bad_request",
                    format!(
                        "could not read the selector `{text}`; write key=value, key!=value or key"
                    ),
                )
            })?;
            Ok(selector)
        })
        .collect()
}

fn duration_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// 系列のディレクトリを、`dumps/` までの途中の段も含めて自分だけが入れる形で
/// 作る。umask に依らず、既にあった段も 700 に閉じ直す。
fn make_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    // 作っている途中で、別の片付け (この unit の別の `start`、兄弟の unit) が空に
    // なった `<ns>/` / `<unit>/` を消すことがある。消されたら作り直す。
    let mut tries = 0;
    loop {
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => break,
            // macOS は消されている途中の段に作ると EINVAL を返す。
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                ) && tries < 3 =>
            {
                tries += 1
            }
            Err(e) => return Err(e),
        }
    }
    // `<dump_id>` / `<ns>` / `<unit>` / `dumps`。
    for level in dir.ancestors().take(4) {
        std::fs::set_permissions(level, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 自分だけが読めるファイルを開く。umask に依らず 600 を付ける。
fn open_private(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::PermissionsExt as _;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .append(append)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// 直下のディレクトリ (symlink は辿らない)。読めなければ空。
fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// UUIDv7 (RFC 9562 §5.7)。先頭 48 bit が Unix ミリ秒なので、文字列の順が
/// 始めた順になる。
fn uuid_v7(now_ms: i64, fill: &mut dyn FnMut(&mut [u8])) -> String {
    let mut bytes = [0u8; 16];
    fill(&mut bytes[6..]);
    let ms = u64::try_from(now_ms).unwrap_or(0).to_be_bytes();
    bytes[..6].copy_from_slice(&ms[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// UUIDv7 の時刻部 (Unix ミリ秒)。UUIDv7 でなければ `None`。
fn uuid_v7_ms(id: &str) -> Option<i64> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || hex.as_bytes().get(12) != Some(&b'7') {
        return None;
    }
    i64::from_str_radix(&hex[..12], 16).ok()
}

#[cfg(test)]
mod tests;
