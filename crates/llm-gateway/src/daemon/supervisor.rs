//! 登録された台を抱えて生かし続ける監督者 (DR-0028 決定 3・4)。
//!
//! やることは 3 つだけ:
//!
//! - 登録簿で `enabled` になっている台を `<binary_path> daemon run <unit>` として起こす
//! - 落ちたら間を置いて起こし直す (`/llm-gateway/healthz` が返れば間隔を戻す)
//! - unix socket で受けた頼み (start / stop / restart / status / reload / log) に答える
//!
//! **設定は読まない**。読むのは子の `daemon run` で、監督者が設定に触るのは
//! 待ち受け先 (healthz の宛先) を知るときだけ。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, broadcast};

use crate::daemon::protocol::{self, LogLine, Request, UnitStatus, Which};
use crate::daemon::registry::{Registry, Unit};

/// 落ちた子を起こし直すまでの、最初の間。
const BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// 起こし直すまでの間の上限。
///
/// 起動即死を繰り返す設定を相手に、無限に詰めても無限に離しても困る。
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// SIGTERM を出してから諦めるまで。過ぎたら SIGKILL。
const TERM_GRACE: Duration = Duration::from_secs(10);

/// 頼まれて起こすとき、起きたと分かるまで待つ上限。
const SPAWN_WAIT: Duration = Duration::from_secs(10);

/// 起こしてから healthz が返るのを待つ上限。
const HEALTH_WAIT: Duration = Duration::from_secs(30);

/// healthz を叩き直す間隔。
///
/// 子は別プロセスで、待ち受けを始めた合図を寄越す口が HTTP しかない。
/// 「起きたことを教えてもらう」経路が無いので、ここだけは叩いて確かめる。
const HEALTH_INTERVAL: Duration = Duration::from_millis(200);

/// 1 台について監督者が覚えていること。
///
/// 望み (`enabled`) は登録簿が正本で、ここに持つのは「今どうなっているか」だけ。
#[derive(Debug, Default)]
struct Watched {
    /// 意図した上げ下げのたびに進む。古い世話係が新しい子を触らないための札。
    epoch: u64,
    /// 世話係が生きているか (= 落ちても起こし直す約束が残っているか)。
    tending: bool,
    pid: Option<u32>,
    since_ms: Option<u64>,
    /// 起こそうとした回数 (成否を問わない)。
    ///
    /// 頼まれて起こすとき、結果が出るまで待つための札。増えていれば
    /// 「起きた」か「起こせなかった」かのどちらかが確定している。
    attempts: u64,
    restarts: u32,
    backoff: Duration,
    last_exit: Option<String>,
}

/// 監督者。
pub struct Supervisor {
    registry: Registry,
    logs: PathBuf,
    socket: PathBuf,
    watched: Mutex<HashMap<String, Watched>>,
    /// 状態が動いたことの合図 (待っている側を起こす)。
    changed: Notify,
    /// 子が書いた行の配り口 (`log --follow`)。
    lines: broadcast::Sender<LogLine>,
}

/// 頼みに答えられなかった理由。CLI がそのまま JSON にする。
#[derive(Debug, Clone)]
pub struct Refused {
    pub kind: &'static str,
    pub message: String,
    pub unit: Option<String>,
}

impl Refused {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            unit: None,
        }
    }

    fn about(kind: &'static str, unit: &str, message: impl Into<String>) -> Self {
        Self {
            unit: Some(unit.to_owned()),
            ..Self::new(kind, message)
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut error = serde_json::Map::new();
        error.insert("kind".to_owned(), serde_json::json!(self.kind));
        error.insert("message".to_owned(), serde_json::json!(self.message));
        if let Some(unit) = &self.unit {
            error.insert("unit".to_owned(), serde_json::json!(unit));
        }
        serde_json::json!({ "error": error })
    }
}

impl From<crate::daemon::registry::Error> for Refused {
    fn from(e: crate::daemon::registry::Error) -> Self {
        Self::new(e.kind(), e.to_string())
    }
}

type Answer = Result<serde_json::Value, Refused>;

impl Supervisor {
    /// 既定の置き場で組み立てる。
    pub fn open() -> Self {
        Self::new(
            Registry::open(),
            protocol::log_dir(),
            protocol::socket_path(),
        )
    }

    pub fn new(registry: Registry, logs: PathBuf, socket: PathBuf) -> Self {
        let (lines, _) = broadcast::channel(1024);
        Self {
            registry,
            logs,
            socket,
            watched: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            lines,
        }
    }

    /// この端末の前で監督する。止められるまで戻らない。
    ///
    /// 先に socket を開いてから子を起こす。逆にすると、起きた直後に来た
    /// 頼みが「監督者が居ない」と断られる。
    pub async fn supervise(self: &Arc<Self>) -> Result<(), String> {
        let listener = self.listen()?;
        self.reload().await;

        let stopped = self.serve(listener);
        tokio::pin!(stopped);
        tokio::select! {
            _ = &mut stopped => {}
            _ = terminated() => {}
        }

        self.shutdown().await;
        let _ = std::fs::remove_file(&self.socket);
        Ok(())
    }

    /// 待ち受けを開く。
    ///
    /// 前回の残骸があれば消してから開く。消さないと `Address already in use`
    /// になり、監督者が二度と上がらない。同時に 2 つ上がるのを止めているのは
    /// OS への登録が 1 つであること (DR-0028 決定 7)。
    fn listen(&self) -> Result<UnixListener, String> {
        if let Some(dir) = self.socket.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not make {}: {e}", dir.display()))?;
        }
        let _ = std::fs::remove_file(&self.socket);
        let listener = UnixListener::bind(&self.socket)
            .map_err(|e| format!("could not listen on {}: {e}", self.socket.display()))?;

        // 頼めば台が動く口なので、他の利用者には開けない。
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("could not close {} to others: {e}", self.socket.display()))?;

        tracing::info!(socket = %self.socket.display(), "supervising");
        Ok(listener)
    }

    async fn serve(self: &Arc<Self>, listener: UnixListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let me = Arc::clone(self);
                    tokio::spawn(async move { me.answer(stream).await });
                }
                Err(e) => {
                    tracing::error!(%e, "stopped accepting requests");
                    return;
                }
            }
        }
    }

    /// 1 本の繋がりに答える。
    async fn answer(self: Arc<Self>, stream: UnixStream) {
        let (reading, mut writing) = stream.into_split();
        let mut lines = BufReader::new(reading).lines();
        let Ok(Some(line)) = lines.next_line().await else {
            return;
        };

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                let refused =
                    Refused::new("bad_request", format!("could not read the request: {e}"));
                let _ = write_line(&mut writing, &refused.to_json()).await;
                return;
            }
        };

        // 追従だけは 1 行で終わらない。
        if let Request::Log(which) = &request {
            self.stream_log(which.clone(), writing).await;
            return;
        }

        let answer = self.handle(request).await;
        let value = match answer {
            Ok(value) => value,
            Err(refused) => refused.to_json(),
        };
        let _ = write_line(&mut writing, &value).await;
    }

    async fn handle(self: &Arc<Self>, request: Request) -> Answer {
        match request {
            Request::Start(which) => self.act(&which, Action::Start).await,
            Request::Stop(which) => self.act(&which, Action::Stop).await,
            Request::Restart(which) => self.act(&which, Action::Restart).await,
            Request::Status(which) => {
                let names = self.choose(&which, true)?;
                Ok(serde_json::json!({ "units": self.status_of(&names).await? }))
            }
            Request::Reload => {
                self.reload().await;
                let names = self.registry.names();
                Ok(serde_json::json!({ "units": self.status_of(&names).await? }))
            }
            // 追従は [`Self::answer`] が先に拾っている。
            Request::Log(_) => Err(Refused::new(
                "bad_request",
                "log is a stream, not a question",
            )),
        }
    }

    fn choose(&self, which: &Which, bare_is_all: bool) -> Result<Vec<String>, Refused> {
        let registered = self.registry.names();
        which
            .choose(&registered, bare_is_all)
            .map_err(|message| Refused {
                kind: if which.unit.is_some() {
                    "unknown_unit"
                } else {
                    "unit_required"
                },
                message,
                unit: which.unit.clone(),
            })
    }

    /// 頼まれた台を、指された順に 1 台ずつ動かす。
    ///
    /// 逆順にするのは、手前 (Caddy) が先の台を優先しているため。後ろから
    /// 順に上げ直せば、外から見た断が出ない (DR-0028 決定 4)。
    async fn act(self: &Arc<Self>, which: &Which, action: Action) -> Answer {
        // 動かす命令は、指されていない台を勝手に動かさない。
        let mut names = self.choose(which, false)?;
        if matches!(action, Action::Restart) {
            names.reverse();
        }

        for name in &names {
            match action {
                Action::Start => self.start(name).await?,
                Action::Stop => self.stop(name).await?,
                Action::Restart => self.restart(name).await?,
            }
        }

        names.sort();
        Ok(serde_json::json!({ "units": self.status_of(&names).await? }))
    }

    /// 居てほしい状態にして、居なければ起こす。
    pub async fn start(self: &Arc<Self>, name: &str) -> Result<(), Refused> {
        self.registry.set_enabled(name, true)?;
        self.tend(name).await;
        Ok(())
    }

    /// 居てほしくない状態にして、居るなら止める。
    pub async fn stop(self: &Arc<Self>, name: &str) -> Result<(), Refused> {
        self.registry.set_enabled(name, false)?;
        self.terminate(name).await;
        Ok(())
    }

    /// 止めてから起こし、healthz が返るまで待つ。
    ///
    /// 待つのは、`--all` のときに前の台が答えないうちに次を落とさないため。
    pub async fn restart(self: &Arc<Self>, name: &str) -> Result<(), Refused> {
        self.registry.set_enabled(name, true)?;
        self.terminate(name).await;
        self.tend(name).await;

        let Some(listen) = self.listen_of(name) else {
            // 待ち受け先が読めない設定は、起きたかどうかを聞けない。
            return Ok(());
        };
        if healthy(&listen, HEALTH_WAIT).await {
            self.reset_backoff(name).await;
            return Ok(());
        }
        Err(Refused::about(
            "health_timeout",
            name,
            format!(
                "`{name}` did not answer /llm-gateway/healthz at {listen} within {}s",
                HEALTH_WAIT.as_secs()
            ),
        ))
    }

    /// 登録簿を読み直して、望みとの差を埋める。
    ///
    /// 定期的に舐めるのではなく、頼まれたときだけ読む (DR-0028 決定 3)。
    pub async fn reload(self: &Arc<Self>) {
        let units = match self.registry.list() {
            Ok(units) => units,
            Err(e) => {
                tracing::error!(%e, "could not read the registry");
                return;
            }
        };

        for (name, unit) in &units {
            if unit.enabled {
                self.tend(name).await;
            } else {
                self.terminate(name).await;
            }
        }

        // 登録簿から消えた台は、抱えたままにしない。
        let known: Vec<String> = {
            let watched = self.watched.lock().await;
            watched.keys().cloned().collect()
        };
        for name in known {
            if !units.iter().any(|(n, _)| *n == name) {
                self.terminate(&name).await;
            }
        }
    }

    /// 世話係を付けて、起こし終わるまで待つ (既に付いていれば何もしない)。
    ///
    /// 待つのは、頼んだ相手に「起こしました」と答えた直後の `status` が
    /// まだ起きていないと言う、を避けるため。
    async fn tend(self: &Arc<Self>, name: &str) {
        let (epoch, attempts) = {
            let mut watched = self.watched.lock().await;
            let state = watched.entry(name.to_owned()).or_default();
            if state.tending {
                return;
            }
            state.epoch += 1;
            state.tending = true;
            state.backoff = BACKOFF_FIRST;
            (state.epoch, state.attempts)
        };
        self.changed.notify_waiters();

        let me = Arc::clone(self);
        let named = name.to_owned();
        tokio::spawn(async move { me.keep(named, epoch).await });

        self.wait_for(name, SPAWN_WAIT, |state| {
            state.pid.is_some() || state.attempts > attempts
        })
        .await;
    }

    /// 1 台を、起こしては見送り続ける。
    async fn keep(self: Arc<Self>, name: String, epoch: u64) {
        loop {
            let unit = match self.registry.get(&name) {
                Ok(unit) => unit,
                Err(e) => {
                    self.gave_up(&name, epoch, e.to_string()).await;
                    return;
                }
            };

            match self.spawn(&name, &unit).await {
                Ok(mut child) => {
                    let pid = child.id().unwrap_or_default();
                    let mine = {
                        let mut watched = self.watched.lock().await;
                        match watched.get_mut(&name) {
                            Some(state) if state.epoch == epoch => {
                                state.pid = Some(pid);
                                state.since_ms = Some(now_ms());
                                state.attempts += 1;
                                true
                            }
                            _ => false,
                        }
                    };
                    // 起こしている間に世話を降ろされていた。放り出さずに
                    // 自分で見送る (誰も知らない子を残さない)。
                    if !mine {
                        signal(pid, libc::SIGTERM);
                        let _ = child.wait().await;
                        return;
                    }
                    self.changed.notify_waiters();
                    tracing::info!(unit = %name, pid, "started");

                    // 起きたと分かったら、次に落ちたときの待ち時間を戻す。
                    if let Some(listen) = self.listen_of(&name) {
                        let me = Arc::clone(&self);
                        let named = name.clone();
                        tokio::spawn(async move {
                            if healthy(&listen, HEALTH_WAIT).await {
                                me.reset_backoff(&named).await;
                            }
                        });
                    }

                    let status = child.wait().await;
                    let ended = match &status {
                        Ok(status) => status.to_string(),
                        Err(e) => format!("could not wait for it: {e}"),
                    };
                    tracing::warn!(unit = %name, status = %ended, "stopped");

                    // 見送ったことは、世話を降ろされていても書く。ここで
                    // 帰ってしまうと、止めたはずの台が居ることになる。
                    let mut watched = self.watched.lock().await;
                    let stale = match watched.get_mut(&name) {
                        Some(state) => {
                            if state.pid == Some(pid) {
                                state.pid = None;
                                state.since_ms = None;
                                state.last_exit = Some(ended);
                            }
                            state.epoch != epoch
                        }
                        None => true,
                    };
                    drop(watched);
                    self.changed.notify_waiters();
                    if stale {
                        return;
                    }
                }
                Err(e) => {
                    tracing::error!(unit = %name, %e, "could not start it");
                    let mut watched = self.watched.lock().await;
                    if let Some(state) = watched.get_mut(&name)
                        && state.epoch == epoch
                    {
                        state.attempts += 1;
                        state.last_exit = Some(e);
                    }
                    drop(watched);
                    self.changed.notify_waiters();
                }
            }

            // 間を置いて起こし直す。世話係を外されていたらここで終わる。
            let wait = {
                let mut watched = self.watched.lock().await;
                let Some(state) = watched.get_mut(&name) else {
                    return;
                };
                if state.epoch != epoch || !state.tending {
                    return;
                }
                let wait = state.backoff;
                state.backoff = (state.backoff * 2).min(BACKOFF_MAX);
                state.restarts += 1;
                wait
            };
            tokio::time::sleep(wait).await;

            let watched = self.watched.lock().await;
            match watched.get(&name) {
                Some(state) if state.epoch == epoch && state.tending => {}
                _ => return,
            }
        }
    }

    /// 起こしようが無かったとき、その理由を残して世話を降りる。
    async fn gave_up(&self, name: &str, epoch: u64, reason: String) {
        let mut watched = self.watched.lock().await;
        if let Some(state) = watched.get_mut(name)
            && state.epoch == epoch
        {
            state.tending = false;
            state.pid = None;
            state.since_ms = None;
            state.last_exit = Some(reason);
        }
        drop(watched);
        self.changed.notify_waiters();
    }

    /// 子を起こす。書いたものはログへ流し、追従にも配る。
    async fn spawn(&self, name: &str, unit: &Unit) -> Result<tokio::process::Child, String> {
        std::fs::create_dir_all(&self.logs)
            .map_err(|e| format!("could not make {}: {e}", self.logs.display()))?;

        let mut child = tokio::process::Command::new(&unit.binary_path)
            .arg("daemon")
            .arg("run")
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", unit.binary_path.display()))?;

        let out = child.stdout.take();
        let err = child.stderr.take();
        let path = protocol::log_path(&self.logs, name);
        let lines = self.lines.clone();
        let named = name.to_owned();
        tokio::spawn(async move { pump(named, path, out, err, lines).await });

        Ok(child)
    }

    /// 今いる子を止める。世話係も外すので、落ちても起こし直さない。
    async fn terminate(&self, name: &str) {
        let pid = {
            let mut watched = self.watched.lock().await;
            let Some(state) = watched.get_mut(name) else {
                return;
            };
            if !state.tending && state.pid.is_none() {
                return;
            }
            state.epoch += 1;
            state.tending = false;
            state.pid
        };
        self.changed.notify_waiters();

        let Some(pid) = pid else { return };
        signal(pid, libc::SIGTERM);
        if self
            .wait_for(name, TERM_GRACE, |state| state.pid.is_none())
            .await
        {
            return;
        }

        // 応答を書き終える猶予は与えた。それでも居るなら落とす。
        tracing::warn!(unit = %name, pid, "did not stop in time, killing it");
        signal(pid, libc::SIGKILL);
        self.wait_for(name, TERM_GRACE, |state| state.pid.is_none())
            .await;
    }

    /// そうなるまで待つ。待ちきれなければ `false`。
    ///
    /// 合図で起きる。定期的に覗きに行くと、間隔の内側で起きた往復を
    /// 取りこぼす (DR-0028 決定 3 と同じ理由)。
    async fn wait_for(
        &self,
        name: &str,
        limit: Duration,
        settled: impl Fn(&Watched) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            // 合図を受ける用意をしてから見る。逆にすると、見てから待つまでの
            // 間に来た合図を取りこぼす。
            let coming = self.changed.notified();
            tokio::pin!(coming);
            {
                let watched = self.watched.lock().await;
                if watched.get(name).is_none_or(&settled) {
                    return true;
                }
            }
            if tokio::time::timeout_at(deadline, coming).await.is_err() {
                return false;
            }
        }
    }

    async fn reset_backoff(&self, name: &str) {
        let mut watched = self.watched.lock().await;
        if let Some(state) = watched.get_mut(name) {
            state.backoff = BACKOFF_FIRST;
        }
    }

    /// 抱えている台を全部止める (監督者が終わるとき)。
    ///
    /// 生かしたまま降りると、次の監督者が拾えない子が残る。
    async fn shutdown(&self) {
        let names: Vec<String> = {
            let watched = self.watched.lock().await;
            watched.keys().cloned().collect()
        };
        for name in &names {
            self.terminate(name).await;
        }
        tracing::info!(units = names.len(), "stopped everything");
    }

    /// 頼まれた台の今。
    pub async fn status_of(&self, names: &[String]) -> Result<Vec<UnitStatus>, Refused> {
        let registered = self.registry.list()?;
        let watched = self.watched.lock().await;

        Ok(registered
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| names.iter().any(|n| n == name))
            .map(|(id, (name, unit))| {
                let state = watched.get(name);
                UnitStatus {
                    id,
                    unit: name.clone(),
                    enabled: unit.enabled,
                    running: state.and_then(|s| s.pid).is_some(),
                    pid: state.and_then(|s| s.pid),
                    since_ms: state.and_then(|s| s.since_ms),
                    restarts: state.map_or(0, |s| s.restarts),
                    last_exit: state.and_then(|s| s.last_exit.clone()),
                }
            })
            .collect())
    }

    /// 書いたものを流し続ける。
    ///
    /// 先に配り口を押さえてからファイルを読む。読んでいる間に書かれた行は
    /// 両方に現れるので、読み終えた長さより手前の行は捨てる。
    async fn stream_log(&self, which: Which, mut writing: tokio::net::unix::OwnedWriteHalf) {
        let names = match self.choose(&which, true) {
            Ok(names) => names,
            Err(refused) => {
                let _ = write_line(&mut writing, &refused.to_json()).await;
                return;
            }
        };
        let mut coming = self.lines.subscribe();

        let mut read: HashMap<String, u64> = HashMap::new();
        for name in &names {
            let path = protocol::log_path(&self.logs, name);
            let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            read.insert(name.clone(), text.len() as u64);
            for line in text.lines() {
                let value = serde_json::json!({"unit": name, "line": line});
                if write_line(&mut writing, &value).await.is_err() {
                    return;
                }
            }
        }

        loop {
            match coming.recv().await {
                Ok(line) => {
                    if !names.contains(&line.unit) {
                        continue;
                    }
                    if read
                        .get(&line.unit)
                        .is_some_and(|read| line.offset <= *read)
                    {
                        continue;
                    }
                    let value = serde_json::json!({"unit": line.unit, "line": line.line});
                    if write_line(&mut writing, &value).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    let value = serde_json::json!({"lost": missed});
                    if write_line(&mut writing, &value).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// 台の待ち受け先 (healthz の宛先)。読めなければ聞かない。
    fn listen_of(&self, name: &str) -> Option<String> {
        let unit = self.registry.get(name).ok()?;
        crate::Config::load(&unit.config)
            .ok()
            .map(|config| config.server.listen)
    }
}

/// 1 台に対してすること。
#[derive(Debug, Clone, Copy)]
enum Action {
    Start,
    Stop,
    Restart,
}

/// 子が書いた行を、ログへ落としつつ配る。
///
/// 2 つの流れを 1 つの係にまとめてから書く。stdout と stderr が別々に同じ
/// ファイルへ書くと、どこまで書いたかを誰も言えなくなり、追従が読んだ分と
/// 突き合わせられない。
async fn pump(
    unit: String,
    path: PathBuf,
    out: Option<tokio::process::ChildStdout>,
    err: Option<tokio::process::ChildStderr>,
    lines: broadcast::Sender<LogLine>,
) {
    use tokio::fs::OpenOptions;
    use tokio::sync::mpsc;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await;
    let Ok(mut file) = file else {
        tracing::error!(unit, path = %path.display(), "could not write the log");
        return;
    };
    let mut offset = file.metadata().await.map(|m| m.len()).unwrap_or_default();

    let (sender, mut written) = mpsc::channel::<String>(256);
    if let Some(out) = out {
        tokio::spawn(read_into(out, sender.clone()));
    }
    if let Some(err) = err {
        tokio::spawn(read_into(err, sender.clone()));
    }
    // 送る側が全部畳んだら終わる。ここが最後の持ち主。
    drop(sender);

    while let Some(line) = written.recv().await {
        let mut text = line.clone();
        text.push('\n');
        if file.write_all(text.as_bytes()).await.is_err() {
            return;
        }
        offset += text.len() as u64;
        // 誰も追従していなければ配り先が無い。それは失敗ではない。
        let _ = lines.send(LogLine {
            unit: unit.clone(),
            line,
            offset,
        });
    }
}

/// 1 つの流れを行に割って渡す。
async fn read_into<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    sender: tokio::sync::mpsc::Sender<String>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if sender.send(line).await.is_err() {
            return;
        }
    }
}

async fn write_line(
    writing: &mut tokio::net::unix::OwnedWriteHalf,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut text = value.to_string();
    text.push('\n');
    writing.write_all(text.as_bytes()).await?;
    writing.flush().await
}

/// healthz が返るまで待つ。返らないまま時間切れなら `false`。
async fn healthy(listen: &str, limit: Duration) -> bool {
    let url = format!("http://{}/llm-gateway/healthz", reachable_authority(listen));
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + limit;

    loop {
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            return true;
        }
        if tokio::time::Instant::now() + HEALTH_INTERVAL >= deadline {
            return false;
        }
        tokio::time::sleep(HEALTH_INTERVAL).await;
    }
}

/// 待ち受けの書き方を、叩ける住所に読み替える。
///
/// `0.0.0.0` は「どこからでも受ける」であって、繋ぎに行く先ではない。
fn reachable_authority(listen: &str) -> String {
    match listen.rsplit_once(':') {
        Some((host, port)) => {
            let host = match host.trim_matches(['[', ']']) {
                "" | "0.0.0.0" | "::" => "127.0.0.1",
                other => other,
            };
            if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            }
        }
        None => listen.to_owned(),
    }
}

fn signal(pid: u32, sig: i32) {
    // 相手は自分が起こした子なので、番号を取り違える余地は無い
    // (待つのは同じ監督者で、番号が回るのは待った後)。
    unsafe { libc::kill(pid as libc::pid_t, sig) };
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// 止められるまで待つ。
async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};

    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("cannot receive SIGTERM");
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = int.recv() => tracing::info!("received SIGINT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::Unit;

    /// 子の代わりに走らせる小さな実行ファイル。
    ///
    /// 本物の `daemon run` は設定も待ち受けも要る。ここで見たいのは
    /// 「起こす / 見送る / 止める」だけなので、同じ形の引数を取って
    /// 言われたとおりに振る舞うだけのものを置く。
    fn fake_binary(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// 走り続ける子。SIGTERM で素直に終わるよう `exec` で置き換わる。
    fn a_long_running_child(dir: &std::path::Path) -> PathBuf {
        fake_binary(
            dir,
            "long",
            "echo \"running $3\"\necho \"to stderr $3\" >&2\nexec sleep 300",
        )
    }

    /// 子が書いた行を、書かれた順に指定の数だけ受ける。
    async fn heard(coming: &mut broadcast::Receiver<LogLine>, count: usize) -> Vec<LogLine> {
        let mut said = Vec::new();
        while said.len() < count {
            let line = tokio::time::timeout(Duration::from_secs(20), coming.recv())
                .await
                .expect("nothing was written")
                .expect("the log was closed");
            said.push(line);
        }
        said
    }

    /// その番号のプロセスがまだ居るか。
    fn is_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    struct World {
        _dir: tempfile::TempDir,
        supervisor: Arc<Supervisor>,
        registry: Registry,
        root: PathBuf,
    }

    fn world() -> World {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let registry = Registry::at(root.join("units"));
        let supervisor = Arc::new(Supervisor::new(
            registry.clone(),
            root.join("logs"),
            root.join("supervisor.sock"),
        ));
        World {
            _dir: dir,
            supervisor,
            registry,
            root,
        }
    }

    impl World {
        /// 1 台を登録する。設定ファイルは置かない (待ち受け先を聞かせない)。
        fn register(&self, name: &str, binary: &std::path::Path, enabled: bool) {
            self.registry
                .add(
                    name,
                    &Unit {
                        config: self.root.join(format!("{name}.toml")),
                        binary_path: binary.to_path_buf(),
                        enabled,
                        added_at: "2026-09-09T00:00:00Z".to_owned(),
                    },
                )
                .unwrap();
        }

        async fn status(&self, name: &str) -> UnitStatus {
            self.supervisor
                .status_of(&[name.to_owned()])
                .await
                .unwrap()
                .remove(0)
        }

        /// そうなるまで待つ。合図で起きるので、覗きに行かない。
        async fn until(&self, name: &str, want: impl Fn(&UnitStatus) -> bool) -> UnitStatus {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                let coming = self.supervisor.changed.notified();
                tokio::pin!(coming);
                let status = self.status(name).await;
                if want(&status) {
                    return status;
                }
                if tokio::time::timeout_at(deadline, coming).await.is_err() {
                    panic!("`{name}` never got there: {status:?}");
                }
            }
        }
    }

    /// `enabled` の台は監督者が起こし、書いたものはログに残る。
    #[tokio::test]
    async fn an_enabled_unit_is_started_and_its_output_is_kept() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, true);

        let mut written = world.supervisor.lines.subscribe();
        world.supervisor.reload().await;
        let status = world.until("stable", |s| s.running).await;
        assert!(status.pid.is_some());
        assert!(status.since_ms.is_some());
        assert_eq!(status.id, 0);
        assert!(status.enabled);

        // 子は `<binary> daemon run <unit>` として起きる。
        let said = heard(&mut written, 2).await;
        assert!(said.iter().any(|l| l.line == "running stable"), "{said:?}");
        // stdout も stderr も同じところへ流す。
        assert!(
            said.iter().any(|l| l.line == "to stderr stable"),
            "{said:?}"
        );
        // 書いたものはファイルにも残る。
        let log = protocol::log_path(&world.root.join("logs"), "stable");
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .contains("running stable")
        );

        world.supervisor.shutdown().await;
    }

    /// `enabled` でない台は、監督者が居ても起きない。
    #[tokio::test]
    async fn a_disabled_unit_is_left_alone() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("quiet", &binary, false);

        world.supervisor.reload().await;
        let status = world.status("quiet").await;
        assert!(!status.running, "{status:?}");
        assert!(!status.enabled);
    }

    /// 落ちたら起こし直す。何度落ちたかも数えている。
    #[tokio::test]
    async fn a_child_that_dies_is_started_again() {
        let world = world();
        // すぐ落ちる子。起こし直されるたびに 1 行増える。
        let binary = fake_binary(&world.root, "flappy", "echo up\nexit 3");
        world.register("flappy", &binary, true);

        let mut written = world.supervisor.lines.subscribe();
        world.supervisor.reload().await;
        let status = world.until("flappy", |s| s.restarts >= 1).await;
        assert!(
            status.last_exit.as_deref().is_some_and(|e| e.contains('3')),
            "how it ended is kept: {status:?}"
        );

        // 起こし直しは本当に起きている (子が 2 度書く)。
        let said = heard(&mut written, 2).await;
        assert_eq!(
            said.iter().filter(|l| l.line == "up").count(),
            2,
            "{said:?}"
        );

        world.supervisor.shutdown().await;
    }

    /// `stop` は望みを下ろしてから止める。落ちても起こし直さない。
    #[tokio::test]
    async fn stopping_a_unit_takes_the_wish_away_first() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, true);
        world.supervisor.reload().await;
        world.until("stable", |s| s.running).await;

        world.supervisor.stop("stable").await.unwrap();

        let status = world.status("stable").await;
        assert!(!status.running, "{status:?}");
        assert!(status.pid.is_none());
        // 望みは登録簿に残る。監督者を上げ直しても起きない。
        assert!(!world.registry.get("stable").unwrap().enabled);

        // 起こし直されないこと。合図が来ないのを待って確かめる。
        let quiet =
            tokio::time::timeout(BACKOFF_FIRST * 3, world.until("stable", |s| s.running)).await;
        assert!(quiet.is_err(), "it came back after being stopped");
    }

    /// `start` は望みを立ててから起こす。2 度頼まれても 1 台のまま。
    #[tokio::test]
    async fn starting_a_unit_writes_the_wish_and_is_idempotent() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, false);

        let supervisor = Arc::clone(&world.supervisor);
        supervisor.start("stable").await.unwrap();
        assert!(world.registry.get("stable").unwrap().enabled);
        let first = world.until("stable", |s| s.running).await;

        supervisor.start("stable").await.unwrap();
        let second = world.status("stable").await;
        assert_eq!(first.pid, second.pid, "the same child is still the one");

        world.supervisor.shutdown().await;
    }

    /// `restart` は止めてから起こす (別の子になる)。
    #[tokio::test]
    async fn restarting_replaces_the_child() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, true);
        world.supervisor.reload().await;
        let before = world.until("stable", |s| s.running).await;

        world.supervisor.restart("stable").await.unwrap();

        let after = world.until("stable", |s| s.running).await;
        assert_ne!(before.pid, after.pid);
        assert!(world.registry.get("stable").unwrap().enabled);

        world.supervisor.shutdown().await;
    }

    /// 監督者が降りるとき、抱えていた子も置き去りにしない。
    #[tokio::test]
    async fn nothing_is_left_behind_when_the_supervisor_stops() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, true);
        world.supervisor.reload().await;
        let running = world.until("stable", |s| s.running).await;

        world.supervisor.shutdown().await;

        assert!(!world.status("stable").await.running);
        // 望みは下ろさない (次の監督者が起こし直せる)。
        assert!(world.registry.get("stable").unwrap().enabled);
        assert!(!is_alive(running.pid.unwrap()), "the child is really gone");
    }

    /// socket 越しの頼みに、1 行の JSON で答える。
    #[tokio::test]
    async fn a_request_over_the_socket_is_answered_in_one_line() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, false);
        let socket = world.root.join("supervisor.sock");

        let listener = world.supervisor.listen().unwrap();
        let serving = Arc::clone(&world.supervisor);
        let accepting = tokio::spawn(async move { serving.serve(listener).await });

        // 動かす前は、居ないと答える。
        let answer = protocol::ask(&socket, &Request::Status(Which::all()))
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value["units"][0]["unit"], serde_json::json!("stable"));
        assert_eq!(value["units"][0]["running"], serde_json::json!(false));

        // 頼めば動き、答えにその結果が載る。
        let answer = protocol::ask(&socket, &Request::Start(Which::named("stable")))
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value["units"][0]["running"], serde_json::json!(true));

        let answer = protocol::ask(&socket, &Request::Stop(Which::named("stable")))
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value["units"][0]["running"], serde_json::json!(false));

        accepting.abort();
        world.supervisor.shutdown().await;
    }

    /// 知らない名前・指されていない頼みは、種別を付けて断る。
    #[tokio::test]
    async fn a_request_that_points_at_nothing_is_refused() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, false);

        let refused = world
            .supervisor
            .handle(Request::Start(Which::named("nope")))
            .await
            .unwrap_err();
        assert_eq!(refused.kind, "unknown_unit");

        let refused = world
            .supervisor
            .handle(Request::Start(Which::default()))
            .await
            .unwrap_err();
        assert_eq!(refused.kind, "unit_required");

        // 数えるだけの頼みは、指されなくても全部として答える。
        let answer = world
            .supervisor
            .handle(Request::Status(Which::default()))
            .await
            .unwrap();
        assert_eq!(answer["units"].as_array().unwrap().len(), 1);
    }

    /// 読めない要求は、繋がりを黙って落とさずに理由を返す。
    #[tokio::test]
    async fn an_unreadable_request_is_answered_with_a_reason() {
        let world = world();
        let socket = world.root.join("supervisor.sock");
        let listener = world.supervisor.listen().unwrap();
        let serving = Arc::clone(&world.supervisor);
        let accepting = tokio::spawn(async move { serving.serve(listener).await });

        let stream = UnixStream::connect(&socket).await.unwrap();
        let (reading, mut writing) = stream.into_split();
        writing.write_all(b"{\"op\":\"fly\"}\n").await.unwrap();
        let mut lines = BufReader::new(reading).lines();
        let answer = lines.next_line().await.unwrap().unwrap();

        let value: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value["error"]["kind"], serde_json::json!("bad_request"));

        accepting.abort();
    }

    /// 追従は、既に書かれた分を出してから、続きを流す。
    #[tokio::test]
    async fn following_the_log_replays_what_is_there_and_then_keeps_going() {
        let world = world();
        let binary = fake_binary(
            &world.root,
            "chatty",
            "echo first\nsleep 0.3\necho second\nexec sleep 300",
        );
        world.register("chatty", &binary, true);
        let socket = world.root.join("supervisor.sock");

        let listener = world.supervisor.listen().unwrap();
        let serving = Arc::clone(&world.supervisor);
        let accepting = tokio::spawn(async move { serving.serve(listener).await });
        world.supervisor.reload().await;

        let stream = UnixStream::connect(&socket).await.unwrap();
        let mut lines = protocol::send(stream, &Request::Log(Which::all()))
            .await
            .unwrap();

        let mut seen = Vec::new();
        while seen.len() < 2 {
            let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
                .await
                .expect("the supervisor kept quiet")
                .unwrap()
                .unwrap();
            seen.push(line);
        }
        assert!(seen[0].contains("first"), "{seen:?}");
        assert!(seen[1].contains("second"), "{seen:?}");
        // 読んだ分は二度出さない。
        assert_eq!(seen.iter().filter(|l| l.contains("first")).count(), 1);

        accepting.abort();
        world.supervisor.shutdown().await;
    }

    /// `--all` の起こし直しは、登録の逆順に 1 台ずつ (DR-0028 決定 4)。
    ///
    /// 手前は先の台を優先しているので、後ろから順に上げ直せば断が出ない。
    #[tokio::test]
    async fn restarting_everything_goes_one_at_a_time_from_the_back() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("stable", &binary, true);
        world.register("unstable", &binary, true);

        // 立ち上がりの分から聞いておく。走り始めた後に聞き始めると、最初の子が
        // 書いた行がまだ届いておらず、起こし直しの分と混ざる。
        let mut written = world.supervisor.lines.subscribe();
        world.supervisor.reload().await;
        let first = world.until("stable", |s| s.running).await;
        let second = world.until("unstable", |s| s.running).await;
        // 1 台につき stdout / stderr の 2 行。読み切ってから頼む。
        heard(&mut written, 4).await;

        world
            .supervisor
            .handle(Request::Restart(Which::all()))
            .await
            .unwrap();

        // 名前順の後ろ (unstable) が先に上がり直す。
        let said = heard(&mut written, 4).await;
        let order: Vec<&str> = said
            .iter()
            .filter(|l| l.line.starts_with("running"))
            .map(|l| l.unit.as_str())
            .collect();
        assert_eq!(order, vec!["unstable", "stable"], "{said:?}");

        // どちらも別の子に入れ替わっている。
        assert_ne!(first.pid, world.status("stable").await.pid);
        assert_ne!(second.pid, world.status("unstable").await.pid);

        world.supervisor.shutdown().await;
    }

    /// 登録簿から消えた台は、抱えたままにしない。
    #[tokio::test]
    async fn a_unit_that_left_the_registry_is_let_go() {
        let world = world();
        let binary = a_long_running_child(&world.root);
        world.register("gone", &binary, true);
        world.supervisor.reload().await;
        let running = world.until("gone", |s| s.running).await;

        world.registry.remove("gone").unwrap();
        world.supervisor.reload().await;

        assert!(!is_alive(running.pid.unwrap()));
        // 登録が無いのだから、状態としても数えない。
        assert!(
            world
                .supervisor
                .status_of(&["gone".to_owned()])
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// healthz が返るまでが「起きた」。返らなければ待ちきって諦める。
    #[tokio::test]
    async fn a_unit_is_up_once_healthz_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/llm-gateway/healthz",
                axum::routing::get(|| async { "ok" }),
            );
            let _ = axum::serve(listener, app).await;
        });

        assert!(healthy(&listen, Duration::from_secs(10)).await);
        // 誰も居ない先は、待っても返らない。
        assert!(!healthy("127.0.0.1:1", Duration::from_millis(400)).await);
    }

    /// 待ち受けの書き方は、そのままでは宛先にならない。
    #[test]
    fn a_listen_address_becomes_something_reachable() {
        assert_eq!(reachable_authority("127.0.0.1:11301"), "127.0.0.1:11301");
        assert_eq!(reachable_authority("0.0.0.0:11301"), "127.0.0.1:11301");
        assert_eq!(reachable_authority("[::]:11301"), "127.0.0.1:11301");
        assert_eq!(reachable_authority("[::1]:11301"), "[::1]:11301");
    }

    /// 断り文句は、種別と対象を持って JSON になる。
    #[test]
    fn a_refusal_names_its_kind_and_unit() {
        let value = Refused::about("health_timeout", "stable", "did not answer").to_json();
        assert_eq!(value["error"]["kind"], serde_json::json!("health_timeout"));
        assert_eq!(value["error"]["unit"], serde_json::json!("stable"));

        let value = Refused::new("unit_required", "say which").to_json();
        assert_eq!(value["error"].get("unit"), None);
    }
}
