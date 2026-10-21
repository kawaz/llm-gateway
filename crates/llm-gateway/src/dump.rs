//! request と response の本文を、時限と容量の上限つきで一時的に書き出す (DR-0038)。
//!
//! 有効化は制御口からだけで (決定 2)、`start` 1 回が 1 つの系列になる (決定 13)。
//! 系列は `dump_id` (UUIDv7) で指し、時限 (`until`)・容量の上限・
//! `--request-only`・書いた量・置き場はすべて系列ごとに持つ。系列の表は
//! メモリにだけ置き、restart で消える (決定 1)。
//!
//! 置き場は `[stats] dir` の `dumps/<unit>/<ns>/<dump_id>/` で、中身は 1 本 1 行の
//! `index.jsonl` と、1 本ごとの `<ts>-<seq>.request.json` / `.response.sse`
//! (決定 6・14)。ディレクトリは 700、ファイルは 600。
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
    /// [`Dumps::active`] と同じもの。止まる時に減らす。
    counter: Arc<AtomicUsize>,
}

/// 系列が止まった理由。答えには載せず、log にだけ出す (決定 11)。
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
        if start.for_ms <= 0 {
            return Err(Refusal::new(
                "bad_request",
                "--for must be longer than zero",
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
    pub fn capture(&self, received: Received<'_>, now: i64) -> Option<Capture> {
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
                // 兄弟の unit の片付けが、しばらく書いていない系列を消していても
                // 書けるよう、毎回置き場を確かめる。
                let written = make_dir(&series.dir)
                    .and_then(|()| write_new(&series.dir.join(&request_file), received.body));
                if let Err(e) = written {
                    warn!(%e, dump_id = %series.id, "could not write a dumped request");
                    return None;
                }
                series.requests.fetch_add(1, Ordering::AcqRel);
                Some(Entry {
                    series,
                    response: None,
                    response_bytes: 0,
                    truncated: false,
                })
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        Some(Capture {
            ts: now,
            seq: received.seq,
            stem,
            request_file,
            request_bytes: size,
            stream: received.stream,
            headers: kept_headers(received.headers),
            entries,
        })
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
pub struct Capture {
    ts: i64,
    seq: u64,
    stem: String,
    request_file: String,
    request_bytes: u64,
    stream: bool,
    headers: Map<String, Value>,
    entries: Vec<Entry>,
}

/// 1 本の、系列 1 つぶんの書き先。
struct Entry {
    series: Arc<Series>,
    /// response のファイル名と、まだ書ける間の書き口。
    response: Option<(String, Option<std::fs::File>)>,
    response_bytes: u64,
    truncated: bool,
}

impl Capture {
    /// upstream が答えた。ここから response のバイト列を書く。
    pub fn respond(mut self, answered: Answered) -> Responding {
        let response_file = format!("{}.response.sse", self.stem);
        for entry in &mut self.entries {
            if entry.series.request_only {
                continue;
            }
            let file = match open_new(&entry.series.dir.join(&response_file)) {
                Ok(file) => Some(file),
                Err(e) => {
                    warn!(%e, dump_id = %entry.series.id, "could not open a dumped response");
                    None
                }
            };
            entry.response = Some((response_file.clone(), file));
        }
        Responding {
            capture: self,
            answered,
            finished: false,
        }
    }

    /// upstream に届かずに gateway が答えた。response のファイルは作らず、
    /// `index.jsonl` の行だけを書く。
    pub fn settle_without_response(self, answered: Answered) {
        Responding {
            capture: self,
            answered,
            finished: false,
        }
        .finish();
    }
}

/// 応答のバイト列を書いている 1 本。
pub struct Responding {
    capture: Capture,
    answered: Answered,
    finished: bool,
}

impl Responding {
    /// 通り過ぎたチャンクをそのまま書く (決定 10)。
    ///
    /// 上限に達したら、収まる分まで書いて打ち切り、その系列を止める (決定 5)。
    ///
    /// Design rationale: チャンクはその場で同期に書く。書き手の task と
    /// channel を挟むと、詰まった時に溜まる分が上限の外でメモリに乗る。
    /// 書くのは page cache までの短い write で、ダンプは調査の間だけ動く。
    pub fn chunk(&mut self, bytes: &[u8]) {
        for entry in &mut self.capture.entries {
            let Some((_, Some(file))) = entry.response.as_mut() else {
                continue;
            };
            let granted = entry.series.reserve_some(bytes.len() as u64);
            let written = usize::try_from(granted).unwrap_or(bytes.len());
            if let Err(e) = file.write_all(&bytes[..written]) {
                warn!(%e, dump_id = %entry.series.id, "could not write a dumped response");
                close(entry);
                continue;
            }
            entry.response_bytes += granted;
            if written < bytes.len() {
                entry.truncated = true;
                close(entry);
                entry.series.finish(StopReason::MaxBytes);
            }
        }
    }

    /// 応答の終端。系列ごとの `index.jsonl` に 1 行足す (決定 14)。2 度目は
    /// 何もしない。
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let (capture, answered) = (&self.capture, &self.answered);
        for entry in &capture.entries {
            let mut line = Map::new();
            line.insert("ts".to_owned(), json!(capture.ts));
            line.insert("seq".to_owned(), json!(capture.seq));
            put(&mut line, "model", answered.model.as_ref());
            put(&mut line, "route", answered.route.as_ref());
            line.insert("status".to_owned(), json!(answered.status));
            put(&mut line, "origin", answered.origin.as_ref());
            line.insert("stream".to_owned(), json!(capture.stream));
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
            line.insert("request_bytes".to_owned(), json!(capture.request_bytes));
            line.insert("request_file".to_owned(), json!(capture.request_file));
            if let Some((name, _)) = &entry.response {
                line.insert("response_bytes".to_owned(), json!(entry.response_bytes));
                line.insert("response_file".to_owned(), json!(name));
            }
            if entry.truncated {
                line.insert("truncated".to_owned(), json!(true));
            }
            line.insert("headers".to_owned(), Value::Object(capture.headers.clone()));
            let mut text = Value::Object(line).to_string();
            text.push('\n');
            if let Err(e) = append(&entry.series.dir.join("index.jsonl"), text.as_bytes()) {
                warn!(%e, dump_id = %entry.series.id, "could not write the dump index");
            }
        }
    }
}

/// response の書き口を閉じる。ファイル名は `index.jsonl` の行に残す。
fn close(entry: &mut Entry) {
    if let Some((_, file)) = entry.response.as_mut() {
        *file = None;
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

/// 自分だけが入れるディレクトリを、途中の段も含めて作る。
fn make_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// 新しいファイルを自分だけが読める形で開く。
fn open_new(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn write_new(path: &Path, body: &[u8]) -> std::io::Result<()> {
    open_new(path)?.write_all(body)
}

/// 1 行を足す。行は 1 回の write で書く (`tail -f` で半端な行を見せない)。
fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?
        .write_all(line)
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

    const NOW: i64 = 1_792_000_000_000;

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
                NOW,
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
        let mut responding = capture.respond(Answered {
            status: 200,
            ..Default::default()
        });
        for chunk in chunks {
            responding.chunk(chunk);
        }
        responding.finish();
        true
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
        assert!(!exchange(&dumps, "lab", 1, b"{}", &[], NOW));
        assert!(!dir.path().join("dumps").exists(), "nothing was made");

        let view = start(&dumps, "lab", 60_000, None);
        assert!(!exchange(&dumps, "other", 2, b"{}", &[], NOW));
        assert!(exchange(
            &dumps,
            "lab",
            3,
            b"{\"a\":1}",
            &[b"event: x\n", b"data: {}\n\n"],
            NOW
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

        assert!(exchange(&dumps, "lab", 1, b"{}", &[], NOW + 999));
        assert!(!exchange(&dumps, "lab", 2, b"{}", &[], NOW + 1_000));

        assert_eq!(index(&view).len(), 1);
        assert!(dumps.status(NOW + 1_000).is_empty());
        assert!(dumps.running(NOW).is_empty(), "stopped for good");
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
        assert!(!exchange(&dumps, "a", 1, b"12345", &[], NOW));
        assert!(dumps.status(NOW).is_empty(), "stopped by the limit");
        assert_eq!(std::fs::read_dir(&refused.dump_dir).unwrap().count(), 0);

        let truncated = start(&dumps, "b", 60_000, Some(10));
        assert!(exchange(
            &dumps,
            "b",
            2,
            b"1234",
            &[b"abc", b"defgh", b"ij"],
            NOW
        ));
        let lines = index(&truncated);
        assert_eq!(lines[0]["truncated"], true);
        assert_eq!(lines[0]["response_bytes"], 6);
        let response =
            Path::new(&truncated.dump_dir).join(lines[0]["response_file"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(response).unwrap(), "abcdef");
        let stopped = dumps.stop(&truncated.dump_id).unwrap();
        assert_eq!(stopped.bytes_written, Some(10));
        assert_eq!(stopped.active, Some(false));
        assert!(
            !exchange(&dumps, "b", 3, b"x", &[], NOW),
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

        assert!(exchange(&dumps, "lab", 7, b"{}", &[b"x"], NOW));

        for view in [&first, &second] {
            assert_eq!(index(view).len(), 1);
        }
        let status = dumps.status(NOW);
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

        let stopped = dumps.stop_all(NOW);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].dump_id, b.dump_id);
        assert!(dumps.stop_all(NOW).is_empty());
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
                    NOW,
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
        assert_eq!(dumps.start(&zero, NOW).unwrap_err().kind, "bad_request");
        let empty = Start {
            for_ms: 1,
            max_bytes: Some(0),
            ..zero
        };
        assert_eq!(dumps.start(&empty, NOW).unwrap_err().kind, "bad_request");
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
        let later = NOW + KEEP_MS * 2;
        let view = start(&dumps, "lab", KEEP_MS * 4, None);
        let aged = std::time::UNIX_EPOCH + std::time::Duration::from_millis(NOW as u64);
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
        let earlier = uuid_v7(NOW, &mut |buf| buf.fill(0xff));
        let later = uuid_v7(NOW + 1, &mut |buf| buf.fill(0));
        assert!(earlier < later);
    }

    /// upstream に届かなかった 1 本は、response のファイルを作らず行だけを書く。
    #[test]
    fn a_request_answered_by_the_gateway_has_only_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let dumps = dumps(dir.path());
        let view = start(&dumps, "lab", 60_000, None);

        dumps
            .capture(received("lab", 9, b"{}"), NOW)
            .unwrap()
            .settle_without_response(Answered {
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
}
