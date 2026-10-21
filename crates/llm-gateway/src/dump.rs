//! request と response の本文を、時限と容量の上限つきで一時的に書き出す (DR-0038)。
//!
//! 有効化は制御口からだけで (決定 2)、`start` 1 回が 1 つの系列になる (決定 13)。
//! 系列は `dump_id` (UUIDv7) で指し、時限 (`until`)・容量の上限・
//! `--request-only`・書いた量・置き場はすべて系列ごとに持つ。系列の表は
//! メモリにだけ置き、restart で消える (決定 1)。
//!
//! 置き場は `[stats] dir` の `dumps/<unit>/<ns>/<dump_id>/` で、中身は 1 本 1 行の
//! `index.jsonl` と、1 本ごとの `<ts>-<seq>.request.json`、upstream の試行ごとの
//! `<ts>-<seq>.<attempt>.response.sse` (決定 6・14)。ディレクトリは 700、
//! ファイルは 600。
//!
//! 書き手は受け口 ([`Dumps::capture`]、gateway が受けた本文) と応答の観測
//! ([`crate::exchange::observe`] の包み、upstream が返したバイト列) の 2 か所で、
//! relay には手を入れない (決定 10)。動いている系列が無い間は atomic の
//! load 1 つで抜け、1 バイトも控えない。
//!
//! 片付けは unit の起動時と `start` の時だけで、48 時間より古い系列を
//! ディレクトリごと消す (決定 9)。

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

/// `--max-bytes` を書かなかった系列の上限 (DR-0038 未決 1)。
///
/// 並列の worker の 1 本が数百 KB なので、10 分の調査で数百本を取っても収まり、
/// 止め忘れても GB には届かない大きさ。
pub const DEFAULT_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// これより古い系列は片付けで消える (決定 9)。
const KEEP_MS: i64 = 48 * 3600 * 1000;

/// `--for` の上限 (決定 1)。片付けの閾値 ([`KEEP_MS`]) の半分以下に抑えるので、
/// 動いている系列のディレクトリの mtime は必ず片付けの閾値より新しく、兄弟の
/// unit の片付けが動いている系列を消すことがない。
pub const MAX_FOR_MS: i64 = 24 * 3600 * 1000;

/// 系列の表と、置き場。
pub struct Dumps {
    /// `<[stats] dir>/dumps`。
    root: PathBuf,
    /// 動いている系列の数。受け口はここだけを見て、0 なら何もしない。
    active: Arc<AtomicUsize>,
    /// 始めた系列の全部 (止まったものも)。止まった系列の `stop` を冪等に
    /// 答えるため、restart まで覚えておく。鍵は UUIDv7 なので始めた順に並ぶ。
    series: Mutex<BTreeMap<String, Arc<Series>>>,
}

/// 1 つの系列 (決定 13)。
struct Series {
    id: String,
    ns: String,
    dir: PathBuf,
    /// 終わりの時刻 (Unix ミリ秒)。
    until: i64,
    max_bytes: u64,
    request_only: bool,
    /// 本文 (request と response のファイル) に書いた合計。`index.jsonl` の
    /// 行は数えない — 打ち切った 1 本にも `truncated: true` の行を残すため。
    bytes_written: AtomicU64,
    /// 書いた本数。
    requests: AtomicU64,
    active: AtomicBool,
    /// 止まった理由。動いている間は `None`。
    stopped_by: Mutex<Option<StopReason>>,
    /// [`Dumps::active`] と同じもの。止まる時に減らす。
    counter: Arc<AtomicUsize>,
}

/// 系列が止まった理由。`start` / `status` / `stop` の答えには載せず log に出し
/// (決定 11)、止まったせいで打ち切った 1 本の `index.jsonl` の行にも `reason` で
/// 残す (決定 5・14)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Expired,
    MaxBytes,
    Stopped,
}

impl StopReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::MaxBytes => "max_bytes",
            Self::Stopped => "stopped",
        }
    }
}

impl Series {
    /// 動いていれば止めて、理由を log に出す。既に止まっていれば何もしない。
    fn finish(&self, reason: StopReason) {
        if self
            .active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            *self.stopped_by.lock().unwrap() = Some(reason);
            self.counter.fetch_sub(1, Ordering::AcqRel);
            info!(
                dump_id = %self.id,
                ns = %self.ns,
                reason = reason.as_str(),
                bytes_written = self.bytes_written.load(Ordering::Acquire),
                requests = self.requests.load(Ordering::Acquire),
                "a dump stopped"
            );
        }
    }

    /// `now` に動いているか。時限を過ぎていればここで止める。
    ///
    /// 時限は timer の発火だけに頼らず、書く時点で壁時計と比べて守る
    /// (決定 1)。スリープで timer が遅れても、過ぎた 1 本は書かない。
    fn live(&self, now: i64) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        if now >= self.until {
            self.finish(StopReason::Expired);
            return false;
        }
        true
    }

    /// 止まった理由。止まった直後に別の書き手が見ても `stopped` と取り違え
    /// ないよう、理由が書かれる前なら `stopped` を返す。
    fn reason(&self) -> StopReason {
        self.stopped_by
            .lock()
            .unwrap()
            .unwrap_or(StopReason::Stopped)
    }

    /// 上限ちょうどまで書いたら止める (決定 5)。超えてからではなく、次の 1 バイトを
    /// 書けなくなった時点で止まる。
    fn stop_if_full(&self) {
        if self.bytes_written.load(Ordering::Acquire) >= self.max_bytes {
            self.finish(StopReason::MaxBytes);
        }
    }

    /// `want` バイトを丸ごと書けるなら、その分を書いたことにして `true`。
    fn reserve_all(&self, want: u64) -> bool {
        self.reserve(|written| {
            written
                .checked_add(want)
                .filter(|after| *after <= self.max_bytes)
                .map(|_| want)
        })
        .is_some()
    }

    /// `want` バイトのうち上限までに収まる分を書いたことにして、その量を返す。
    fn reserve_some(&self, want: u64) -> u64 {
        self.reserve(|written| Some(want.min(self.max_bytes.saturating_sub(written))))
            .unwrap_or(0)
    }

    /// 書いた量に `grant(書いた量)` を足す。`None` なら何も足さない。
    ///
    /// 並列の 1 本どうしが同じ系列に書くので、読んでから足すまでの間に
    /// 割り込まれたらやり直す。
    fn reserve(&self, grant: impl Fn(u64) -> Option<u64>) -> Option<u64> {
        let mut written = self.bytes_written.load(Ordering::Acquire);
        loop {
            let granted = grant(written)?;
            match self.bytes_written.compare_exchange_weak(
                written,
                written + granted,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(granted),
                Err(now) => written = now,
            }
        }
    }

    fn view(&self, detail: Detail) -> View {
        let counts = !matches!(detail, Detail::Started);
        View {
            dump_id: self.id.clone(),
            dump_dir: self.dir.display().to_string(),
            ns: self.ns.clone(),
            until: self.until,
            max_bytes: self.max_bytes,
            request_only: self.request_only,
            bytes_written: counts.then(|| self.bytes_written.load(Ordering::Acquire)),
            requests: counts.then(|| self.requests.load(Ordering::Acquire)),
            active: matches!(detail, Detail::Stopped).then(|| self.active.load(Ordering::Acquire)),
        }
    }
}

/// 答えにどこまで載せるか (決定 11)。
#[derive(Clone, Copy)]
enum Detail {
    /// `start`: 引数の全部。
    Started,
    /// `status`: それに書いた量と本数。
    Running,
    /// `stop`: さらに `active`。
    Stopped,
}

/// 系列 1 つの答え (決定 11)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    pub dump_id: String,
    /// 系列のディレクトリの絶対パス。
    pub dump_dir: String,
    pub ns: String,
    pub until: i64,
    pub max_bytes: u64,
    pub request_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// `/llm-gateway/self` と `daemon status` に載せる 1 件 (決定 12)。
///
/// 止め忘れに気づくための欄で、本文の置き場は載せない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Running {
    pub dump_id: String,
    pub ns: String,
    pub until: i64,
    pub bytes_written: u64,
}

/// `start` で頼まれたこと。
#[derive(Debug, Clone)]
pub struct Start<'a> {
    pub unit: &'a str,
    pub ns: &'a str,
    /// 時限の長さ (ミリ秒)。
    pub for_ms: i64,
    pub max_bytes: Option<u64>,
    pub request_only: bool,
}

/// 頼みを断る理由。`kind` は制御口の答えの `error.kind` になる (決定 2)。
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
        // 答えに載せるのは絶対パス (決定 6)。設定に相対で書かれていても、
        // 突き合わせる人が置き場を導出し直さずに済むよう、ここで直しておく。
        let root = std::path::absolute(stats_dir)
            .unwrap_or_else(|_| stats_dir.to_path_buf())
            .join("dumps");
        Self {
            root,
            active: Arc::new(AtomicUsize::new(0)),
            series: Mutex::new(BTreeMap::new()),
        }
    }

    /// 系列を 1 つ始める (決定 1・13)。始める前に古い系列を片付ける (決定 9)。
    ///
    /// 時限の timer はこの unit が持つ (tokio の上で呼ばれた時だけ張る)。
    /// timer が遅れても、書く側が `until` と壁時計を比べて守る。
    pub fn start(self: &Arc<Self>, start: &Start<'_>, now: i64) -> Result<View, Refusal> {
        if !crate::config::is_file_name_safe(start.ns) {
            return Err(Refusal::new(
                "invalid_namespace",
                format!(
                    "namespace `{}` cannot be dumped: its name becomes a directory, so it may contain only letters, digits, `.`, `_` and `-`, and must not start with `.`",
                    start.ns
                ),
            ));
        }
        if start.for_ms <= 0 || start.for_ms > MAX_FOR_MS {
            return Err(Refusal::new(
                "invalid_duration",
                "--for must be longer than zero and at most 24h (a dump is temporary, and dumps older than 48 hours are removed)",
            ));
        }
        if start.max_bytes == Some(0) {
            return Err(Refusal::new(
                "bad_request",
                "--max-bytes must be larger than zero",
            ));
        }
        self.sweep(now);

        let id = uuid_v7(now, &mut |buf| rand::fill(buf));
        let dir = self.root.join(start.unit).join(start.ns).join(&id);
        make_dir(&dir).map_err(|e| {
            Refusal::new(
                "dump_dir_unwritable",
                format!("could not make {}: {e}", dir.display()),
            )
        })?;
        let series = Arc::new(Series {
            id: id.clone(),
            ns: start.ns.to_owned(),
            dir,
            until: now.saturating_add(start.for_ms),
            max_bytes: start.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            request_only: start.request_only,
            bytes_written: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            active: AtomicBool::new(true),
            stopped_by: Mutex::new(None),
            counter: Arc::clone(&self.active),
        });
        self.series
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::clone(&series));
        self.active.fetch_add(1, Ordering::AcqRel);
        info!(
            dump_id = %id,
            ns = %series.ns,
            until = series.until,
            max_bytes = series.max_bytes,
            request_only = series.request_only,
            dir = %series.dir.display(),
            "a dump started"
        );

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let wait = std::time::Duration::from_millis(start.for_ms.unsigned_abs());
            let expiring = Arc::downgrade(&series);
            runtime.spawn(async move {
                tokio::time::sleep(wait).await;
                if let Some(series) = Weak::upgrade(&expiring) {
                    series.finish(StopReason::Expired);
                }
            });
        }
        Ok(series.view(Detail::Started))
    }

    /// 系列を時限を待たずに止める。既に止まっていても同じ形で答える (決定 3)。
    pub fn stop(&self, dump_id: &str) -> Result<View, Refusal> {
        let series = self.series.lock().unwrap().get(dump_id).cloned();
        let Some(series) = series else {
            return Err(Refusal::new(
                "unknown_dump",
                format!(
                    "this unit has no dump `{dump_id}` (a restart forgets every dump). `dump status` lists the running ones"
                ),
            ));
        };
        series.finish(StopReason::Stopped);
        Ok(series.view(Detail::Stopped))
    }

    /// 動いている系列を全部止め、止めた分を返す。
    pub fn stop_all(&self, now: i64) -> Vec<View> {
        self.live(now)
            .into_iter()
            .map(|series| {
                series.finish(StopReason::Stopped);
                series.view(Detail::Stopped)
            })
            .collect()
    }

    /// 動いている系列の一覧 (決定 11)。
    pub fn status(&self, now: i64) -> Vec<View> {
        self.live(now)
            .iter()
            .map(|series| series.view(Detail::Running))
            .collect()
    }

    /// `/llm-gateway/self` に載せる、動いている系列 (決定 12)。
    pub fn running(&self, now: i64) -> Vec<Running> {
        self.live(now)
            .iter()
            .map(|series| Running {
                dump_id: series.id.clone(),
                ns: series.ns.clone(),
                until: series.until,
                bytes_written: series.bytes_written.load(Ordering::Acquire),
            })
            .collect()
    }

    fn live(&self, now: i64) -> Vec<Arc<Series>> {
        if self.active.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        self.series
            .lock()
            .unwrap()
            .values()
            .filter(|series| series.live(now))
            .cloned()
            .collect()
    }

    /// 48 時間より古い系列をディレクトリごと消す (決定 9)。消した数を返す。
    ///
    /// 古さは系列のディレクトリの mtime で見る。中にファイルが作られるたびに
    /// 進むので「最後に 1 本書いてから 48 時間」になる。掃くのは `dumps/` の
    /// 下全体 (他の unit の分も)。この unit で動いている系列は、しばらく 1 本も
    /// 来ていなくても消さない。空になった `<ns>/` と `<unit>/` も消す。
    pub fn sweep(&self, now: i64) -> usize {
        let running: Vec<PathBuf> = self
            .live(now)
            .iter()
            .map(|series| series.dir.clone())
            .collect();
        let mut removed = 0;
        for unit in subdirectories(&self.root) {
            for ns in subdirectories(&unit) {
                for series in subdirectories(&ns) {
                    if running.contains(&series) || !older_than(&series, now - KEEP_MS) {
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
            info!(removed, root = %self.root.display(), "removed dumps older than 48 hours");
        }
        removed
    }

    /// 受けた 1 本を、その ns で動いている系列の全部に書く (決定 10・13)。
    ///
    /// 動いている系列が無ければ atomic の load 1 つで `None`。どの系列に
    /// 書くかはここで決まり、途中から始めた系列にこの 1 本は入らない。
    /// 書けた系列が 1 つも無ければ `None`。
    pub fn capture(&self, received: Received<'_>, now: i64) -> Option<Arc<Capture>> {
        if self.active.load(Ordering::Acquire) == 0 {
            return None;
        }
        let targets: Vec<Arc<Series>> = self
            .series
            .lock()
            .unwrap()
            .values()
            .filter(|series| series.ns == received.ns && series.live(now))
            .cloned()
            .collect();
        if targets.is_empty() {
            return None;
        }

        let stem = format!("{now}-{}", received.seq);
        let request_file = format!("{stem}.request.json");
        let size = received.body.len() as u64;
        let entries: Vec<Entry> = targets
            .into_iter()
            .filter_map(|series| {
                // 書くと上限を超えるなら書かずに止まる (決定 5)。
                if !series.reserve_all(size) {
                    series.finish(StopReason::MaxBytes);
                    return None;
                }
                let written = write_into(&series.dir, &request_file, received.body);
                if let Err(e) = written {
                    warn!(%e, dump_id = %series.id, "could not write a dumped request");
                    return None;
                }
                series.requests.fetch_add(1, Ordering::AcqRel);
                series.stop_if_full();
                Some(Entry {
                    series,
                    responses: Vec::new(),
                })
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        Some(Arc::new(Capture {
            ts: now,
            seq: received.seq,
            stem,
            request_file,
            request_bytes: size,
            stream: received.stream,
            headers: kept_headers(received.headers),
            attempts: Mutex::new(Vec::new()),
            entries: Mutex::new(entries),
            adopted: AtomicU64::new(0),
            settled: AtomicBool::new(false),
        }))
    }
}

/// 受け口が受けた 1 本 (gateway が書き換える前の形、決定 7)。
pub struct Received<'a> {
    pub ns: &'a str,
    /// unit の通し番号 ([`crate::exchange::request_span_numbered`] のもの)。
    pub seq: u64,
    /// 受けた本文そのまま。
    pub body: &'a [u8],
    /// 受けたヘッダ。秘密のもの (決定 8) はここで落とす。
    pub headers: &'a [(String, String)],
    pub stream: bool,
}

/// 1 本がどう答えられたか。`index.jsonl` の行の欄 (決定 14)。
///
/// upstream に届かずに gateway が断った 1 本 (経路が尽きた等) は、分からない欄を
/// `None` のまま書く。
#[derive(Debug, Clone, Default)]
pub struct Answered {
    pub status: u16,
    pub model: Option<String>,
    pub route: Option<String>,
    pub origin: Option<String>,
    pub credential: Option<String>,
    pub cache_strategy: Option<String>,
    pub cache_ttl_secs: Option<u64>,
}

/// request を書き終えて、応答を待っている 1 本。
///
/// response は upstream から読んだバイト列を Wire が解釈する前に [`Self::tee`]
/// で書き (決定 7・10)、`index.jsonl` の行は応答の終端で [`Self::settle`] が書く。
pub struct Capture {
    ts: i64,
    seq: u64,
    stem: String,
    request_file: String,
    request_bytes: u64,
    stream: bool,
    headers: Map<String, Value>,
    /// upstream が答えた試行。経路を替えて試し直すたびに 1 つ増える。
    attempts: Mutex<Vec<Attempt>>,
    entries: Mutex<Vec<Entry>>,
    /// クライアントへ返した試行の番号。0 は「upstream の応答を返していない」。
    adopted: AtomicU64,
    settled: AtomicBool,
}

/// upstream の 1 回の答え (経路を試した 1 回)。
#[derive(Debug, Clone)]
pub struct Attempt {
    /// 1 から数える。ファイル名の `<ts>-<seq>.<attempt>.response.sse` の番号。
    pub attempt: u64,
    pub route: String,
    pub credential: Option<String>,
    /// upstream が返した status。クライアントへ返した status とは違うことがある。
    pub status: u16,
}

/// 答えが決まり、応答の終端を待っている 1 本。
pub struct Settling {
    capture: Arc<Capture>,
    answered: Answered,
}

impl Settling {
    /// 応答の終端で `index.jsonl` に行を書く。
    pub fn finish(&self) {
        self.capture.settle(&self.answered);
    }
}

/// 1 本の、系列 1 つぶんの書き先。
struct Entry {
    series: Arc<Series>,
    /// 試行ごとの response。`--request-only` の系列では空のまま。
    responses: Vec<Written>,
}

/// 1 回の試行の response を、系列 1 つに書いた分。
struct Written {
    attempt: u64,
    file_name: String,
    /// まだ書ける間の書き口。
    file: Option<std::fs::File>,
    bytes: u64,
    /// 打ち切った理由。最後まで書けた試行は `None`。
    cut: Option<StopReason>,
}

impl Written {
    /// 書き口を閉じ、打ち切った理由を残す。ファイル名は行に残す。
    fn cut(&mut self, reason: StopReason) {
        self.file = None;
        self.cut = Some(reason);
    }

    fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("response_file".to_owned(), json!(self.file_name));
        fields.insert("response_bytes".to_owned(), json!(self.bytes));
        if let Some(reason) = self.cut {
            fields.insert("truncated".to_owned(), json!(true));
            fields.insert("reason".to_owned(), json!(reason.as_str()));
        }
        fields
    }
}

impl Capture {
    /// upstream から読んだ本文を、そのまま流しながら系列ごとのファイルに書く
    /// (決定 7・10)。Wire が方言を通訳する前、gateway が本文を集め直したり
    /// エラーの本文に差し替えたりする前のバイト列。
    ///
    /// 経路を試すたびに呼ばれ、試行ごとに別のファイル
    /// (`<ts>-<seq>.<attempt>.response.sse`) に書く。前の試行の分は消さず、
    /// 系列の量からも差し引かない — gateway は先に断られた応答を持ったまま次を
    /// 試し、後の応答を捨てて先の応答を返すことがあるので、どの試行を返すかは
    /// ここでは分からない。返した試行は gateway が [`Self::adopt`] で伝える。
    ///
    /// `attempt` は gateway が送るたびに振る番号 (1 から)。
    pub fn tee(
        self: &Arc<Self>,
        attempt: u64,
        route: &str,
        credential: Option<&str>,
        status: u16,
        body: crate::egress::BodyStream,
    ) -> crate::egress::BodyStream {
        use futures_util::StreamExt as _;

        self.attempts.lock().unwrap().push(Attempt {
            attempt,
            route: route.to_owned(),
            credential: credential.map(str::to_owned),
            status,
        });
        self.open(attempt, crate::credential::time::now_unix_ms());
        let me = Arc::clone(self);
        body.map(move |item| {
            if let Ok(chunk) = &item {
                me.write(attempt, chunk, crate::credential::time::now_unix_ms());
            }
            item
        })
        .boxed()
    }

    /// 試行 `attempt` の response のファイルを、系列ごとに開く。
    fn open(&self, attempt: u64, now: i64) {
        let file_name = format!("{}.{attempt}.response.sse", self.stem);
        for entry in self.entries.lock().unwrap().iter_mut() {
            if entry.series.request_only {
                continue;
            }
            let file = match open_in(&entry.series.dir, &file_name) {
                Ok(file) => Some(file),
                Err(e) => {
                    warn!(%e, dump_id = %entry.series.id, "could not open a dumped response");
                    None
                }
            };
            let mut written = Written {
                attempt,
                file_name: file_name.clone(),
                file,
                bytes: 0,
                cut: None,
            };
            // 応答が届いた時点で系列が止まっていれば、1 バイトも書かずに打ち切る。
            if !entry.series.live(now) {
                written.cut(entry.series.reason());
            }
            entry.responses.push(written);
        }
    }

    /// 試行 `attempt` のチャンクをそのまま書く (決定 10)。
    ///
    /// 系列が止まっていれば (時限・`stop`・上限) 以後は書かずに打ち切る (決定 5)。
    /// 上限に掛かるチャンクは収まる分まで書いて打ち切り、系列を止める。
    ///
    /// Design rationale: チャンクはその場で同期に書く。書き手の task と
    /// channel を挟むと、詰まった時に溜まる分が上限の外でメモリに乗る。
    /// 書くのは page cache までの短い write で、ダンプは調査の間だけ動く。
    fn write(&self, attempt: u64, bytes: &[u8], now: i64) {
        for entry in self.entries.lock().unwrap().iter_mut() {
            let series = Arc::clone(&entry.series);
            let Some(written) = entry
                .responses
                .iter_mut()
                .find(|written| written.attempt == attempt)
            else {
                continue;
            };
            if written.file.is_none() {
                continue;
            }
            if !series.live(now) {
                written.cut(series.reason());
                continue;
            }
            let granted = series.reserve_some(bytes.len() as u64);
            let length = usize::try_from(granted).unwrap_or(bytes.len());
            let Some(file) = written.file.as_mut() else {
                continue;
            };
            if let Err(e) = file.write_all(&bytes[..length]) {
                warn!(%e, dump_id = %series.id, "could not write a dumped response");
                written.file = None;
                continue;
            }
            written.bytes += granted;
            if length < bytes.len() {
                written.cut(StopReason::MaxBytes);
                series.finish(StopReason::MaxBytes);
            } else {
                series.stop_if_full();
            }
        }
    }

    /// gateway がクライアントへ返すと決めた試行。決めた時点で gateway が呼ぶ。
    pub fn adopt(&self, attempt: u64) {
        self.adopted.store(attempt, Ordering::Release);
    }

    /// upstream が答えた。行は応答の終端で [`Settling::finish`] が書く。
    pub fn answered(self: Arc<Self>, answered: Answered) -> Settling {
        Settling {
            capture: self,
            answered,
        }
    }

    /// 応答の終端。系列ごとの `index.jsonl` に 1 行足す (決定 14)。2 度目は
    /// 何もしない。
    ///
    /// 行は 1 request 1 行で、upstream の試行を `attempts` に全部並べ、クライアントへ
    /// 返した試行の番号 ([`Self::adopt`]) を `adopted` に書く。gateway が自分で組んだ
    /// エラーを返した 1 本は `adopted` が `null`。採用した試行の response の欄は、
    /// 行の最上位にも写す。
    pub fn settle(&self, answered: &Answered) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        let attempts = self.attempts.lock().unwrap().clone();
        let adopted = match self.adopted.load(Ordering::Acquire) {
            0 => None,
            attempt => Some(attempt),
        };
        for entry in self.entries.lock().unwrap().iter_mut() {
            let mut line = Map::new();
            line.insert("ts".to_owned(), json!(self.ts));
            line.insert("seq".to_owned(), json!(self.seq));
            put(&mut line, "model", answered.model.as_ref());
            put(&mut line, "route", answered.route.as_ref());
            line.insert("status".to_owned(), json!(answered.status));
            put(&mut line, "origin", answered.origin.as_ref());
            line.insert("stream".to_owned(), json!(self.stream));
            put(&mut line, "credential", answered.credential.as_ref());
            put(
                &mut line,
                "cache_strategy",
                answered.cache_strategy.as_ref(),
            );
            put(
                &mut line,
                "cache_ttl_secs",
                answered.cache_ttl_secs.as_ref(),
            );
            line.insert("request_bytes".to_owned(), json!(self.request_bytes));
            line.insert("request_file".to_owned(), json!(self.request_file));
            let listed: Vec<Value> = attempts
                .iter()
                .map(|attempt| {
                    let mut item = Map::new();
                    item.insert("attempt".to_owned(), json!(attempt.attempt));
                    item.insert("route".to_owned(), json!(attempt.route));
                    put(&mut item, "credential", attempt.credential.as_ref());
                    item.insert("status".to_owned(), json!(attempt.status));
                    if let Some(written) = entry
                        .responses
                        .iter()
                        .find(|written| written.attempt == attempt.attempt)
                    {
                        item.extend(written.fields());
                    }
                    Value::Object(item)
                })
                .collect();
            line.insert("attempts".to_owned(), Value::Array(listed));
            line.insert("adopted".to_owned(), json!(adopted));
            if let Some(written) = adopted.and_then(|adopted| {
                entry
                    .responses
                    .iter()
                    .find(|written| written.attempt == adopted)
            }) {
                line.extend(written.fields());
            }
            line.insert("headers".to_owned(), Value::Object(self.headers.clone()));
            for written in &mut entry.responses {
                written.file = None;
            }
            let mut text = Value::Object(line).to_string();
            text.push('\n');
            if let Err(e) = append(&entry.series.dir.join("index.jsonl"), text.as_bytes()) {
                warn!(%e, dump_id = %entry.series.id, "could not write the dump index");
            }
        }
    }
}

/// 値があれば欄にする。無ければ欄ごと出さない。
fn put<T: Serialize>(line: &mut Map<String, Value>, key: &str, value: Option<&T>) {
    if let Some(value) = value {
        line.insert(key.to_owned(), json!(value));
    }
}

/// `index.jsonl` の行に載せるヘッダ (DR-0038 未決 2)。
///
/// 秘密のもの (keepalive の控えと同じ述語、決定 8) だけを落とし、残りは全部
/// 残す。`anthropic-beta` や `x-claude-code-*` のように突き合わせに要るものを
/// 選り分けない。同じ名前が複数あれば `, ` で繋ぐ。
fn kept_headers(headers: &[(String, String)]) -> Map<String, Value> {
    let mut kept: Map<String, Value> = Map::new();
    for (name, value) in headers {
        if crate::gateway::is_authorization(name) {
            continue;
        }
        let name = name.to_ascii_lowercase();
        match kept.get_mut(&name) {
            Some(Value::String(joined)) => {
                joined.push_str(", ");
                joined.push_str(value);
            }
            _ => {
                kept.insert(name, json!(value));
            }
        }
    }
    kept
}

/// 系列のディレクトリを、`dumps/` までの途中の段も含めて自分だけが入れる形で
/// 作る。既にあった段も 700 に閉じ直す (作る時の mode は既存の段に効かない)。
fn make_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // `<dump_id>` / `<ns>` / `<unit>` / `dumps`。
    for level in dir.ancestors().take(4) {
        std::fs::set_permissions(level, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 新しいファイルを自分だけが読める形で開く。
fn open_new(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// 系列のディレクトリに新しいファイルを開く。ディレクトリが消えていた時だけ
/// 作り直して閉じ直す (権限を正すのは系列を作る時と作り直す時だけ)。
fn open_in(dir: &Path, name: &str) -> std::io::Result<std::fs::File> {
    match open_new(&dir.join(name)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            make_dir(dir)?;
            open_new(&dir.join(name))
        }
        opened => opened,
    }
}

/// 系列のディレクトリに新しいファイルを書く ([`open_in`])。
fn write_into(dir: &Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    open_in(dir, name)?.write_all(body)
}

/// 1 行を足す。行は 1 回の write で書く (`tail -f` で半端な行を見せない)。
///
/// 既にあったファイルも 600 に閉じる (作る時の mode は既存のファイルに効かない)。
fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(line)
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

/// mtime が `cutoff` (Unix ミリ秒) より前か。読めなければ消さない側に倒す。
fn older_than(dir: &Path, cutoff: i64) -> bool {
    std::fs::symlink_metadata(dir)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .is_some_and(|ms| ms < cutoff)
}

/// UUIDv7 (RFC 9562 §5.7)。先頭 48 bit が Unix ミリ秒なので、文字列の順が
/// 始めた順になる (決定 13)。
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

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use futures_util::{FutureExt as _, StreamExt as _};

    /// 試験の「今」。応答のチャンクは書く時点の壁時計で時限を見るので、
    /// 本物の今に揃える。
    fn now() -> i64 {
        static NOW: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        *NOW.get_or_init(crate::credential::time::now_unix_ms)
    }

    fn dumps(dir: &Path) -> Arc<Dumps> {
        Arc::new(Dumps::new(dir))
    }

    fn start(dumps: &Arc<Dumps>, ns: &str, for_ms: i64, max_bytes: Option<u64>) -> View {
        dumps
            .start(
                &Start {
                    unit: "u",
                    ns,
                    for_ms,
                    max_bytes,
                    request_only: false,
                },
                now(),
            )
            .unwrap()
    }

    fn received<'a>(ns: &'a str, seq: u64, body: &'a [u8]) -> Received<'a> {
        Received {
            ns,
            seq,
            body,
            headers: &[],
            stream: true,
        }
    }

    /// 1 本を受けて応答を書き切る。書いた系列が無ければ `false`。
    /// upstream が `chunks` を返したことにして、tee を通して読み切る。
    fn drain(capture: &Arc<Capture>, chunks: &[&[u8]]) {
        let body: crate::egress::BodyStream = futures_util::stream::iter(
            chunks
                .iter()
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )
        .boxed();
        let read: Vec<_> = capture
            .tee(1, "r", None, 200, body)
            .collect::<Vec<_>>()
            .now_or_never()
            .expect("an in-memory stream is ready");
        assert_eq!(read.len(), chunks.len(), "every chunk still flows");
    }

    /// 1 本を受けて応答を書き切る。書いた系列が無ければ `false`。
    fn exchange(
        dumps: &Dumps,
        ns: &str,
        seq: u64,
        request: &[u8],
        chunks: &[&[u8]],
        now: i64,
    ) -> bool {
        let Some(capture) = dumps.capture(received(ns, seq, request), now) else {
            return false;
        };
        drain(&capture, chunks);
        capture.adopt(1);
        capture.settle(&answered());
        true
    }

    /// 試験の upstream (`r`) が答えたことにした答え。返したのは試行 1 として
    /// 呼び手が [`Capture::adopt`] する。
    fn answered() -> Answered {
        Answered {
            status: 200,
            route: Some("r".to_owned()),
            ..Default::default()
        }
    }

    fn index(view: &View) -> Vec<Value> {
        std::fs::read_to_string(Path::new(&view.dump_dir).join("index.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// 動いている系列が無ければ何も書かない。系列は ns ごとで、別の ns の 1 本は入らない。
    #[test]
    fn only_the_namespace_of_a_running_dump_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        assert!(!exchange(&dumps, "lab", 1, b"{}", &[], now()));
        assert!(!dir.path().join("dumps").exists(), "nothing was made");

        let view = start(&dumps, "lab", 60_000, None);
        assert!(!exchange(&dumps, "other", 2, b"{}", &[], now()));
        assert!(exchange(
            &dumps,
            "lab",
            3,
            b"{\"a\":1}",
            &[b"event: x\n", b"data: {}\n\n"],
            now()
        ));

        let lines = index(&view);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["seq"], 3);
        assert_eq!(lines[0]["request_bytes"], 7);
        assert_eq!(lines[0]["response_bytes"], 19);
        let response = Path::new(&view.dump_dir).join(lines[0]["response_file"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(response).unwrap(),
            "event: x\ndata: {}\n\n"
        );
        assert_eq!(
            Path::new(&view.dump_dir),
            dir.path().join("dumps/u/lab").join(&view.dump_id)
        );
    }

    /// `until` を過ぎた 1 本は、timer が遅れていても書かない (決定 1)。
    /// 過ぎた系列は status からも消える。
    #[test]
    fn a_request_after_until_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 1_000, None);

        assert!(exchange(&dumps, "lab", 1, b"{}", &[], now() + 999));
        assert!(!exchange(&dumps, "lab", 2, b"{}", &[], now() + 1_000));

        assert_eq!(index(&view).len(), 1);
        assert!(dumps.status(now() + 1_000).is_empty());
        assert!(dumps.running(now()).is_empty(), "stopped for good");
        assert_eq!(dumps.active.load(Ordering::Acquire), 0);
    }

    /// 時限の timer は unit が持ち、`until` で系列を止める。
    #[tokio::test]
    async fn the_timer_stops_a_dump_at_its_until() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        start(&dumps, "lab", 20, None);
        assert_eq!(dumps.active.load(Ordering::Acquire), 1);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(dumps.active.load(Ordering::Acquire), 0);
    }

    /// 書くと上限を超える request は書かずに止まる。response は収まる分まで
    /// 書いて打ち切り、行に `truncated: true` が付く (決定 5)。
    #[test]
    fn the_size_limit_stops_a_dump_without_writing_past_it() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());

        let refused = start(&dumps, "a", 60_000, Some(4));
        assert!(!exchange(&dumps, "a", 1, b"12345", &[], now()));
        assert!(dumps.status(now()).is_empty(), "stopped by the limit");
        assert_eq!(std::fs::read_dir(&refused.dump_dir).unwrap().count(), 0);

        let truncated = start(&dumps, "b", 60_000, Some(10));
        assert!(exchange(
            &dumps,
            "b",
            2,
            b"1234",
            &[b"abc", b"defgh", b"ij"],
            now()
        ));
        let lines = index(&truncated);
        assert_eq!(lines[0]["truncated"], true);
        assert_eq!(lines[0]["reason"], "max_bytes");
        assert_eq!(lines[0]["response_bytes"], 6);
        let response =
            Path::new(&truncated.dump_dir).join(lines[0]["response_file"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(response).unwrap(), "abcdef");
        let stopped = dumps.stop(&truncated.dump_id).unwrap();
        assert_eq!(stopped.bytes_written, Some(10));
        assert_eq!(stopped.active, Some(false));
        assert!(
            !exchange(&dumps, "b", 3, b"x", &[], now()),
            "it stays stopped"
        );
    }

    /// 同じ ns に重なる 2 系列には、同じ 1 本が両方書かれる (決定 13)。
    #[test]
    fn overlapping_dumps_both_get_the_same_request() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let first = start(&dumps, "lab", 60_000, None);
        let second = start(&dumps, "lab", 60_000, None);
        assert_ne!(first.dump_id, second.dump_id);

        assert!(exchange(&dumps, "lab", 7, b"{}", &[b"x"], now()));

        for view in [&first, &second] {
            assert_eq!(index(view).len(), 1);
        }
        let status = dumps.status(now());
        assert_eq!(status.len(), 2);
        assert!(
            status
                .iter()
                .all(|v| v.requests == Some(1) && v.bytes_written == Some(3))
        );
    }

    /// `stop` は冪等で、既に止まった系列にも `active: false` で答える。知らない
    /// id は `unknown_dump`。`stop_all` は動いている分だけを止めて返す (決定 3・11)。
    #[test]
    fn stopping_is_idempotent_and_unknown_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let a = start(&dumps, "a", 60_000, None);
        let b = start(&dumps, "b", 60_000, None);

        assert_eq!(dumps.stop(&a.dump_id).unwrap().active, Some(false));
        assert_eq!(dumps.stop(&a.dump_id).unwrap().active, Some(false));
        assert_eq!(dumps.stop("nope").unwrap_err().kind, "unknown_dump");

        let stopped = dumps.stop_all(now());
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].dump_id, b.dump_id);
        assert!(dumps.stop_all(now()).is_empty());
        assert_eq!(dumps.active.load(Ordering::Acquire), 0);
    }

    /// ディレクトリ名になる ns は、置き場の外を指せない名前だけを通す (決定 6)。
    #[test]
    fn a_namespace_that_cannot_be_a_directory_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        for ns in ["..", ".hidden", "a/b", ""] {
            let refused = dumps
                .start(
                    &Start {
                        unit: "u",
                        ns,
                        for_ms: 1,
                        max_bytes: None,
                        request_only: false,
                    },
                    now(),
                )
                .unwrap_err();
            assert_eq!(refused.kind, "invalid_namespace", "{ns}");
        }
        let zero = Start {
            unit: "u",
            ns: "lab",
            for_ms: 0,
            max_bytes: None,
            request_only: false,
        };
        assert_eq!(
            dumps.start(&zero, now()).unwrap_err().kind,
            "invalid_duration"
        );
        let too_long = Start {
            for_ms: MAX_FOR_MS + 1,
            ..zero.clone()
        };
        assert_eq!(
            dumps.start(&too_long, now()).unwrap_err().kind,
            "invalid_duration"
        );
        let longest = Start {
            ns: "longest",
            for_ms: MAX_FOR_MS,
            ..zero.clone()
        };
        assert!(
            dumps.start(&longest, now()).is_ok(),
            "24h itself is allowed"
        );
        let empty = Start {
            for_ms: 1,
            max_bytes: Some(0),
            ..zero
        };
        assert_eq!(dumps.start(&empty, now()).unwrap_err().kind, "bad_request");
    }

    /// 片付けは 48 時間より古い系列だけをディレクトリごと消し、空になった
    /// `<ns>/` と `<unit>/` も消す。他の unit の分も掃く (決定 9)。
    #[test]
    fn sweeping_removes_only_dumps_older_than_48_hours() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dumps");
        let old = root.join("other/lab/0190-old");
        let fresh = root.join("other/lab/0190-fresh");
        let lonely = root.join("gone/x/0190-old");
        for path in [&old, &fresh, &lonely] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("index.jsonl"), "{}\n").unwrap();
        }
        let now = crate::credential::time::now_unix_ms();
        let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(49 * 3600);
        for path in [&old, &lonely] {
            std::fs::File::open(path)
                .unwrap()
                .set_modified(aged)
                .unwrap();
        }

        let dumps = dumps(dir.path());
        assert_eq!(dumps.sweep(now), 2);

        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(
            !root.join("gone").exists(),
            "empty ns and unit directories go too"
        );
    }

    /// この unit で動いている系列は、1 本も来ないまま 48 時間経っても消さない。
    /// 片付けで消えた置き場にも、次の 1 本は書ける。
    #[test]
    fn a_running_dump_survives_the_sweep_and_its_directory_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let later = now() + 3600 * 1000;
        let view = start(&dumps, "lab", MAX_FOR_MS, None);
        let aged = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis((now() - KEEP_MS - 3600 * 1000) as u64);
        std::fs::File::open(&view.dump_dir)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        assert_eq!(dumps.sweep(later), 0);
        assert!(Path::new(&view.dump_dir).exists());

        std::fs::remove_dir_all(&view.dump_dir).unwrap();
        assert!(exchange(&dumps, "lab", 1, b"{}", &[], later));
        assert_eq!(index(&view).len(), 1);
    }

    /// `start` も片付けの契機になる (決定 9)。
    #[test]
    fn starting_a_dump_sweeps_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("dumps/u/lab/0190-old");
        std::fs::create_dir_all(&old).unwrap();
        let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(49 * 3600);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        let dumps = dumps(dir.path());
        dumps
            .start(
                &Start {
                    unit: "u",
                    ns: "lab",
                    for_ms: 1_000,
                    max_bytes: None,
                    request_only: false,
                },
                crate::credential::time::now_unix_ms(),
            )
            .unwrap();

        assert!(!old.exists());
    }

    /// 秘密のヘッダは keepalive の控えと同じ述語で落とし、残りは全部残す (決定 8)。
    #[test]
    fn secret_headers_are_dropped_and_the_rest_kept() {
        let headers = [
            ("authorization".to_owned(), "Bearer t".to_owned()),
            ("x-api-key".to_owned(), "k".to_owned()),
            ("cookie".to_owned(), "c".to_owned()),
            ("proxy-authorization".to_owned(), "p".to_owned()),
            ("anthropic-beta".to_owned(), "a".to_owned()),
            ("anthropic-beta".to_owned(), "b".to_owned()),
            ("user-agent".to_owned(), "claude-cli".to_owned()),
        ];
        assert_eq!(
            Value::Object(kept_headers(&headers)),
            json!({"anthropic-beta": "a, b", "user-agent": "claude-cli"})
        );
    }

    /// UUIDv7 は時刻を先頭に持ち、版と変種の bit が立つ (決定 13)。
    #[test]
    fn a_dump_id_is_a_uuid_v7_ordered_by_time() {
        let id = uuid_v7(0x0190_0000_0001, &mut |buf| buf.fill(0xff));
        assert_eq!(id, "01900000-0001-7fff-bfff-ffffffffffff");
        let earlier = uuid_v7(now(), &mut |buf| buf.fill(0xff));
        let later = uuid_v7(now() + 1, &mut |buf| buf.fill(0));
        assert!(earlier < later);
    }

    /// upstream に届かなかった 1 本は、response のファイルを作らず行だけを書く。
    #[test]
    fn a_request_answered_by_the_gateway_has_only_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 60_000, None);

        dumps
            .capture(received("lab", 9, b"{}"), now())
            .unwrap()
            .settle(&Answered {
                status: 503,
                model: Some("m".to_owned()),
                ..Default::default()
            });

        let lines = index(&view);
        assert_eq!(lines[0]["status"], 503);
        assert_eq!(lines[0]["model"], "m");
        assert!(lines[0].get("response_file").is_none());
        assert_eq!(std::fs::read_dir(&view.dump_dir).unwrap().count(), 2);
    }

    /// 上限ちょうどまで書いたら止まる。最後まで収まった 1 本は打ち切りではなく、
    /// その後のチャンクから `reason: "max_bytes"` で打ち切る (決定 5)。
    #[test]
    fn reaching_the_limit_exactly_stops_the_dump() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());

        let exact = start(&dumps, "a", 60_000, Some(6));
        assert!(exchange(&dumps, "a", 1, b"12", &[b"abcd"], now()));
        assert!(dumps.status(now()).is_empty(), "stopped at the limit");
        let line = &index(&exact)[0];
        assert!(line.get("truncated").is_none(), "{line}");

        let over = start(&dumps, "b", 60_000, Some(6));
        assert!(exchange(&dumps, "b", 2, b"12", &[b"abcd", b"e"], now()));
        let line = &index(&over)[0];
        assert_eq!(line["truncated"], true);
        assert_eq!(line["reason"], "max_bytes");
        assert_eq!(line["response_bytes"], 4);

        let request_fills = start(&dumps, "c", 60_000, Some(2));
        assert!(exchange(&dumps, "c", 3, b"12", &[b"x"], now()));
        let line = &index(&request_fills)[0];
        assert_eq!(line["reason"], "max_bytes", "{line}");
        assert_eq!(line["response_bytes"], 0);
    }

    /// 止めた系列には、流れている途中の応答も以後のチャンクを書かず、行に
    /// `truncated: true` と `reason: "stopped"` が付く (決定 5)。
    #[test]
    fn stopping_cuts_a_response_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 60_000, None);
        let capture = dumps.capture(received("lab", 1, b"{}"), now()).unwrap();

        let body: crate::egress::BodyStream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"first")),
            Ok(Bytes::from_static(b"second")),
        ])
        .boxed();
        let mut teed = capture.tee(1, "r", None, 200, body);
        teed.next().now_or_never().unwrap().unwrap().unwrap();
        dumps.stop(&view.dump_id).unwrap();
        let rest = teed.next().now_or_never().unwrap().unwrap().unwrap();
        assert_eq!(&rest[..], b"second", "the client still gets everything");
        capture.adopt(1);
        capture.settle(&answered());

        let line = &index(&view)[0];
        assert_eq!(line["truncated"], true);
        assert_eq!(line["reason"], "stopped");
        assert_eq!(line["response_bytes"], 5);
    }

    /// 時限を過ぎた系列も、流れている途中の応答を書かずに `reason: "expired"` で
    /// 打ち切る。チャンクを書く時点の壁時計で見る (決定 1)。
    #[test]
    fn expiry_cuts_a_response_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        // 時限が今ちょうどで終わる系列。受けたのはその手前。
        let started = crate::credential::time::now_unix_ms() - 10_000;
        let view = dumps
            .start(
                &Start {
                    unit: "u",
                    ns: "lab",
                    for_ms: 10_000,
                    max_bytes: None,
                    request_only: false,
                },
                started,
            )
            .unwrap();
        let capture = dumps
            .capture(received("lab", 1, b"{}"), started + 9_999)
            .unwrap();

        drain(&capture, &[b"late"]);
        capture.adopt(1);
        capture.settle(&answered());

        let line = &index(&view)[0];
        assert_eq!(line["truncated"], true);
        assert_eq!(line["reason"], "expired");
        assert_eq!(line["response_bytes"], 0);
    }

    /// 経路を替えて試し直したら、試行ごとに別のファイルに書き、前の試行も消さず
    /// 系列の量からも差し引かない。行には試行を全部並べ、gateway が返すと決めた
    /// 試行を `adopted` に書く。
    #[test]
    fn every_attempt_is_kept_and_the_adopted_one_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 60_000, None);
        let capture = dumps.capture(received("lab", 1, b"{}"), now()).unwrap();

        let earlier: crate::egress::BodyStream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"denied")),
            Ok(Bytes::from_static(b"-rest")),
        ])
        .boxed();
        let mut earlier = capture.tee(1, "a", Some("cred-a"), 429, earlier);
        earlier.next().now_or_never().unwrap().unwrap().unwrap();
        let later: crate::egress::BodyStream =
            futures_util::stream::iter(vec![Ok(Bytes::from_static(b"answer"))]).boxed();
        capture
            .tee(2, "b", None, 200, later)
            .collect::<Vec<_>>()
            .now_or_never()
            .unwrap();
        earlier.next().now_or_never().unwrap().unwrap().unwrap();
        capture.adopt(2);
        capture.settle(&Answered {
            status: 200,
            route: Some("b".to_owned()),
            ..Default::default()
        });

        let line = &index(&view)[0];
        assert_eq!(line["adopted"], 2);
        assert_eq!(
            line["response_file"],
            "{}-1.2.response.sse".replace("{}", &line["ts"].to_string())
        );
        assert_eq!(line["attempts"][0]["route"], "a");
        assert_eq!(line["attempts"][0]["credential"], "cred-a");
        assert_eq!(line["attempts"][0]["status"], 429);
        assert_eq!(line["attempts"][0]["response_bytes"], 11);
        assert_eq!(line["attempts"][1]["status"], 200);
        let read = |name: &Value| {
            std::fs::read_to_string(Path::new(&view.dump_dir).join(name.as_str().unwrap())).unwrap()
        };
        assert_eq!(read(&line["attempts"][0]["response_file"]), "denied-rest");
        assert_eq!(read(&line["attempts"][1]["response_file"]), "answer");
        assert_eq!(dumps.status(now())[0].bytes_written, Some(2 + 11 + 6));
    }

    /// gateway が自分で組んだエラーを返した 1 本は `adopted: null`。upstream が
    /// 1 度も答えていなければ `attempts` は空で、response の欄も無い。
    #[test]
    fn an_answer_the_gateway_made_adopts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 60_000, None);
        let capture = dumps.capture(received("lab", 1, b"{}"), now()).unwrap();
        capture.settle(&Answered {
            status: 503,
            ..Default::default()
        });

        let line = &index(&view)[0];
        assert_eq!(line["adopted"], Value::Null);
        assert_eq!(line["attempts"], json!([]));
        assert!(line.get("response_file").is_none(), "{line}");
    }

    /// 前からあった `index.jsonl` は書く時に 600 に、前からあったディレクトリは系列を
    /// 始める時に 700 に閉じ直す (権限を正すのは系列を作る時と作り直す時だけ)。
    #[test]
    fn existing_files_and_directories_are_closed_again() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let open_dir = std::fs::Permissions::from_mode(0o755);
        let ns_dir = dir.path().join("dumps/u/lab");
        std::fs::create_dir_all(&ns_dir).unwrap();
        for level in ns_dir.ancestors().take(3) {
            std::fs::set_permissions(level, open_dir.clone()).unwrap();
        }
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 60_000, None);
        let series = Path::new(&view.dump_dir);
        for level in series.ancestors().take(4) {
            assert_eq!(mode(level), 0o700, "{}", level.display());
        }

        let index_path = series.join("index.jsonl");
        std::fs::write(&index_path, "").unwrap();
        std::fs::set_permissions(&index_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(exchange(&dumps, "lab", 1, b"{}", &[b"x"], now()));
        assert_eq!(mode(&index_path), 0o600);

        // 消えた置き場は作り直し、その時に閉じる。
        std::fs::remove_dir_all(series).unwrap();
        std::fs::set_permissions(&ns_dir, open_dir).unwrap();
        assert!(exchange(&dumps, "lab", 2, b"{}", &[b"x"], now()));
        assert_eq!(mode(series), 0o700);
        assert_eq!(mode(&ns_dir), 0o700);
    }
}
