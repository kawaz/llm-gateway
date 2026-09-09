//! OS への常駐登録 (DR-0028 決定 7)。
//!
//! 登録するのは監督者 1 つだけ。どの台を抱えるかは登録簿の話で、OS は知らない。
//!
//! 何を書いて何を叩くかは [`platform`] が組み立て、ここは「書く / 叩く / 待つ」
//! だけを持つ。分けてあるので `--dry-run` が本番と同じ物を見せられる。

pub mod platform;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use llm_gateway::daemon::protocol::{self, LogLine, Request, Which};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::failure::Failure;
use crate::help;
use crate::options::split;
use crate::service::platform::{Env, Kind, Plan, Step};

/// 監督者が畳み終わるのを待つ上限。
///
/// 監督者は SIGTERM を受けてから子を 1 台ずつ止めるので、台数ぶんの猶予が要る。
const STOP_WAIT: Duration = Duration::from_secs(15);

/// 命令を実際に叩く人。
///
/// 試験は本物の `launchctl` を呼ばない (呼べば手元の常駐が動く)。叩いた並びを
/// 検めるために、ここだけ差し替えられるようにしてある。
pub trait Runner {
    fn run(&self, step: &Step) -> Result<Output, Failure>;
}

/// 叩いた結果。
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 本物を叩く。
pub struct System;

impl Runner for System {
    fn run(&self, step: &Step) -> Result<Output, Failure> {
        let out = std::process::Command::new(&step.program)
            .args(&step.args)
            .output()
            .map_err(|e| {
                Failure::new(
                    "command_failed",
                    format!("could not run `{}`: {e}", step.program),
                )
            })?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

pub fn dispatch(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::SERVICE);
        return Ok(ExitCode::SUCCESS);
    }
    let rest = &args[1..];
    match args[0].as_str() {
        "register" => {
            let dry_run = flag(rest, "dry-run")?;
            register(&here()?, &System, dry_run)
        }
        "unregister" => {
            no_options(rest)?;
            unregister(&here()?, &System)
        }
        "start" => {
            no_options(rest)?;
            start(&here()?, &System)
        }
        "stop" => {
            no_options(rest)?;
            stop(&here()?, &System)
        }
        "status" => {
            no_options(rest)?;
            status(&here()?, &System, running_units())
        }
        "log" => log(&log_path(), flag(rest, "follow")?),
        other => Err(Failure::from(format!(
            "there is no `service {other}` command. see `llm-gateway service --help`"
        ))),
    }
}

/// この端末での登録の形。
fn here() -> Result<Plan, Failure> {
    Ok(platform::plan(platform::kind(), &env()?))
}

fn env() -> Result<Env, Failure> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Failure::from("HOME is not set, so I cannot tell where to register"))?;

    // 走らせるのは今の自分。`daemon add` が台ごとの binary を焼き込むのと同じで、
    // 「登録した時点のもの」を指す (どのビルドを常駐させたかが後から分かる)。
    let exe = std::env::current_exe()
        .map_err(|e| Failure::from(format!("could not find my own path: {e}")))?;

    // OS が起こす監督者は shell を通らない。手元の面 (XDG の指し先) を渡さないと、
    // 別の状態ディレクトリを見たまま上がる。PATH は渡さない — 走らせる binary は
    // 登録簿が絶対パスで持っており、探す必要がないので。
    let mut environment = Vec::new();
    for key in ["XDG_STATE_HOME", "XDG_CONFIG_HOME"] {
        if let Some(value) = std::env::var_os(key)
            && let Some(value) = value.to_str()
        {
            environment.push((key.to_owned(), value.to_owned()));
        }
    }

    let unit_dir = match platform::kind() {
        Kind::Launchd => home.join("Library").join("LaunchAgents"),
        Kind::Systemd => home.join(".config").join("systemd").join("user"),
    };

    Ok(Env {
        exe,
        unit_dir,
        log: log_path(),
        uid: uid(&home),
        environment,
    })
}

/// 自分の uid。launchd の domain (`gui/<uid>`) を指すのに要る。
///
/// Design rationale: std に `getuid` は無い。libc を足すほどの用ではないので、
/// 自分の home ディレクトリの所有者から読む (自分の home は自分のもの)。
fn uid(home: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(home).map(|m| m.uid()).unwrap_or(0)
}

fn log_path() -> PathBuf {
    protocol::log_dir().join("supervise.log")
}

/// OS に載せる。
///
/// `--dry-run` なら、書く中身と叩く並びを出すだけで何も触らない。本番の手前で
/// 「何が起きるか」を全部見られるようにしてある (移行の runbook がこれを使う)。
pub fn register(plan: &Plan, runner: &dyn Runner, dry_run: bool) -> Result<ExitCode, Failure> {
    if dry_run {
        println!(
            "{}",
            json!({
                "dry_run": true,
                "label": plan.label,
                "path": plan.unit_path.display().to_string(),
                "contents": plan.unit_text,
                "commands": plan.register.iter().map(Step::to_json).collect::<Vec<_>>(),
            })
        );
        return Ok(ExitCode::SUCCESS);
    }

    if plan.unit_path.exists() {
        return Err(Failure::new(
            "already_registered",
            format!(
                "`{}` is already registered at {}",
                plan.label,
                plan.unit_path.display()
            ),
        )
        .with("hint", "run `llm-gateway service unregister` first"));
    }

    if let Some(dir) = plan.unit_path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Failure::from(format!("could not make {}: {e}", dir.display())))?;
    }
    // 監督者が書く先が無いと、OS 側が起動そのものに失敗する。
    if let Some(dir) = plan.log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&plan.unit_path, &plan.unit_text)
        .map_err(|e| Failure::from(format!("could not write {}: {e}", plan.unit_path.display())))?;

    if let Err(failure) = run_all(runner, &plan.register) {
        // 載せられなかった unit ファイルを残すと、次の `register` が
        // `already_registered` で断る。書く前の姿に戻す。
        let _ = std::fs::remove_file(&plan.unit_path);
        return Err(failure);
    }

    println!(
        "{}",
        json!({
            "registered": true,
            "label": plan.label,
            "path": plan.unit_path.display().to_string(),
        })
    );
    Ok(ExitCode::SUCCESS)
}

/// OS から降ろす。
pub fn unregister(plan: &Plan, runner: &dyn Runner) -> Result<ExitCode, Failure> {
    if !plan.unit_path.exists() {
        return Err(Failure::new(
            "not_registered",
            format!("`{}` is not registered", plan.label),
        ));
    }
    // 降ろせなかったのに消すと、載ったままのものを指すファイルが無くなる。
    run_all(runner, &plan.unregister)?;
    std::fs::remove_file(&plan.unit_path).map_err(|e| {
        Failure::from(format!(
            "could not remove {}: {e}",
            plan.unit_path.display()
        ))
    })?;
    run_all(runner, &plan.after_remove)?;

    println!("{}", json!({"registered": false, "label": plan.label}));
    Ok(ExitCode::SUCCESS)
}

pub fn start(plan: &Plan, runner: &dyn Runner) -> Result<ExitCode, Failure> {
    registered(plan)?;
    run_all(runner, &plan.start)?;
    println!("{}", json!({"label": plan.label, "started": true}));
    Ok(ExitCode::SUCCESS)
}

/// 止めて、監督者が畳み終わるまで待つ。
///
/// 「命令が戻った = 止まった」ではない。監督者は SIGTERM を受けてから子を
/// 1 台ずつ止めるので、戻った直後はまだ全部生きていることがある。
pub fn stop(plan: &Plan, runner: &dyn Runner) -> Result<ExitCode, Failure> {
    registered(plan)?;
    let gone = wait_while_stopping(&protocol::socket_path(), || run_all(runner, &plan.stop))?;
    if !gone {
        return Err(Failure::new(
            "stop_timeout",
            format!(
                "`{}` was told to stop but was still holding its socket after {}s",
                plan.label,
                STOP_WAIT.as_secs()
            ),
        ));
    }
    println!("{}", json!({"label": plan.label, "stopped": true}));
    Ok(ExitCode::SUCCESS)
}

/// 止める前に頼み口を掴んでおき、それが切れるのを待つ。
///
/// Design rationale: socket ファイルの有無を舐めて待つと、間隔に根拠が無いうえ
/// 「消して作り直した」を取りこぼす。掴んだ繋がりは監督者が終わった瞬間に
/// EOF になるので、待つ側は何も測らずに済む。
fn wait_while_stopping(
    socket: &Path,
    tell: impl FnOnce() -> Result<(), Failure>,
) -> Result<bool, Failure> {
    let runtime = runtime()?;
    runtime.block_on(async move {
        let held = tokio::net::UnixStream::connect(socket).await.ok();
        tell()?;
        // 掴めなかったのなら、そもそも監督者は答えていない。
        let Some(mut held) = held else {
            return Ok(true);
        };
        let closed = async move {
            let mut buffer = [0u8; 64];
            while let Ok(read) = held.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
            }
        };
        Ok(tokio::time::timeout(STOP_WAIT, closed).await.is_ok())
    })
}

/// 登録されているか、動いているか、そして何を抱えているか。
///
/// 3 つは別の問いで、答えられる相手も違う。登録は unit ファイル、OS から見た
/// 生死は launchctl / systemctl、抱えている台は監督者しか知らない。
pub fn status(
    plan: &Plan,
    runner: &dyn Runner,
    instances: Option<Value>,
) -> Result<ExitCode, Failure> {
    let registered = plan.unit_path.exists();
    let service = match runner.run(&plan.status) {
        Ok(out) => service_status(plan.kind, &out),
        // 聞けないこと自体は答えのうち。登録の有無までは言える。
        Err(_) => json!({"loaded": false, "running": false}),
    };
    let units = instances
        .as_ref()
        .and_then(|value| value.get("units").cloned())
        .unwrap_or_else(|| json!([]));

    println!(
        "{}",
        json!({
            "registered": registered,
            // 監督者に届いたかどうかが「動いている」の答え。OS が loaded と
            // 言っていても、頼み口が開いていなければ頼めない。
            "running": instances.is_some(),
            "pid": service.get("pid").cloned().unwrap_or(Value::Null),
            "label": plan.label,
            "path": plan.unit_path.display().to_string(),
            "service": service,
            "instances": units,
        })
    );
    Ok(ExitCode::SUCCESS)
}

/// OS の答えから、載っているか・動いているかを読む。
fn service_status(kind: Kind, out: &Output) -> Value {
    // 知らない label を聞かれた launchctl / systemctl は非 0 で断る。
    if out.code != 0 && out.stdout.trim().is_empty() {
        return json!({"loaded": false, "running": false});
    }
    match kind {
        Kind::Launchd => launchd_status(&out.stdout),
        Kind::Systemd => systemd_status(&out.stdout),
    }
}

fn launchd_status(text: &str) -> Value {
    let mut status = json!({"loaded": true, "running": false});
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "state" => status["running"] = json!(value == "running"),
            "pid" => {
                if let Ok(pid) = value.parse::<u32>() {
                    status["pid"] = json!(pid);
                }
            }
            "last exit code" => status["last_exit"] = json!(value),
            _ => {}
        }
    }
    status
}

fn systemd_status(text: &str) -> Value {
    let mut status = json!({"loaded": false, "running": false});
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "LoadState" => status["loaded"] = json!(value == "loaded"),
            "ActiveState" => status["running"] = json!(value == "active"),
            "MainPID" => match value.parse::<u32>() {
                Ok(0) | Err(_) => {}
                Ok(pid) => status["pid"] = json!(pid),
            },
            "ExecMainStatus" => status["last_exit"] = json!(value),
            _ => {}
        }
    }
    status
}

/// 監督者に「何を抱えているか」を聞く。居なければ `None`。
fn running_units() -> Option<Value> {
    let socket = protocol::socket_path();
    if !socket.exists() {
        return None;
    }
    let runtime = tokio::runtime::Runtime::new().ok()?;
    let answer = runtime
        .block_on(async move { protocol::ask(&socket, &Request::Status(Which::all())).await })
        .ok()?;
    let value: Value = serde_json::from_str(&answer).ok()?;
    // 断られた答えは「抱えているもの」ではない。
    if value.get("error").is_some() {
        return None;
    }
    Some(value)
}

/// 監督者が書いたものを出す。
///
/// 子のログ (`daemon log`) と違って監督者を通さない。監督者自身の stdout は
/// OS が受けてファイルへ流しており、読むのに監督者は要らない (むしろ死んだ
/// 理由を読むのは監督者が居ないときである)。
pub fn log(path: &Path, follow: bool) -> Result<ExitCode, Failure> {
    if !follow {
        return dump(path);
    }
    follow_log(path)
}

fn dump(path: &Path) -> Result<ExitCode, Failure> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        // 1 度も上がっていなければ何も書かれていない。異常ではない。
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(ExitCode::SUCCESS),
        Err(e) => Err(Failure::from(format!(
            "could not read {}: {e}",
            path.display()
        ))),
    }
}

/// 書かれ続けるものを流す (答えは JSONL、DR-0028 決定 8)。
///
/// Design rationale: ファイルが伸びたことを知る手立てを自前で持たない。
/// `tail -F` は OS の通知 (kqueue / inotify) で待つので、間隔を決めて舐める
/// 必要がなく、まだ無いファイルが現れるのも待てる。
fn follow_log(path: &Path) -> Result<ExitCode, Failure> {
    let path = path.to_path_buf();
    runtime()?.block_on(async move {
        let mut child = tokio::process::Command::new("tail")
            .arg("-n")
            .arg("+1")
            .arg("-F")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Failure::from(format!("could not follow {}: {e}", path.display())))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Failure::from("could not read what tail prints"))?;

        let mut lines = BufReader::new(stdout).lines();
        let mut offset = 0u64;
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| Failure::from(format!("stopped reading {}: {e}", path.display())))?
        {
            offset += line.len() as u64 + 1;
            println!(
                "{}",
                json!(LogLine {
                    unit: "supervise".to_owned(),
                    line,
                    offset,
                })
            );
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn registered(plan: &Plan) -> Result<(), Failure> {
    if plan.unit_path.exists() {
        return Ok(());
    }
    Err(Failure::new(
        "not_registered",
        format!("`{}` is not registered", plan.label),
    )
    .with("hint", "run `llm-gateway service register` first"))
}

fn run_all(runner: &dyn Runner, steps: &[Step]) -> Result<(), Failure> {
    for step in steps {
        let out = runner.run(step)?;
        if out.code != 0 {
            let line = std::iter::once(step.program.clone())
                .chain(step.args.clone())
                .collect::<Vec<_>>()
                .join(" ");
            return Err(Failure::new(
                "command_failed",
                format!(
                    "`{line}` exited with {}: {}",
                    out.code,
                    out.stderr.trim().lines().next().unwrap_or("(said nothing)")
                ),
            ));
        }
    }
    Ok(())
}

/// `--<name>` が居るか。他のものが混ざっていれば断る。
fn flag(args: &[String], name: &str) -> Result<bool, Failure> {
    let mut found = false;
    for arg in args {
        match split(arg) {
            Some((key, None)) if key == name => found = true,
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    Ok(found)
}

fn no_options(args: &[String]) -> Result<(), Failure> {
    match args.first() {
        Some(unexpected) => Err(Failure::from(format!(
            "could not understand `{unexpected}`"
        ))),
        None => Ok(()),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 叩かれた並びを覚えるだけの人。本物の launchctl は呼ばない。
    #[derive(Default)]
    struct Recorder {
        calls: RefCell<Vec<String>>,
        answer: Output,
    }

    impl Recorder {
        fn saying(answer: Output) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                answer,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Runner for Recorder {
        fn run(&self, step: &Step) -> Result<Output, Failure> {
            self.calls.borrow_mut().push(
                std::iter::once(step.program.clone())
                    .chain(step.args.clone())
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            Ok(self.answer.clone())
        }
    }

    /// 何を叩いても断る人。
    struct Refuses;

    impl Runner for Refuses {
        fn run(&self, _: &Step) -> Result<Output, Failure> {
            Ok(Output {
                code: 64,
                stdout: String::new(),
                stderr: "Bootstrap failed: 5: Input/output error\n".to_owned(),
            })
        }
    }

    fn plan_in(dir: &Path) -> Plan {
        platform::plan(
            Kind::Launchd,
            &Env {
                exe: PathBuf::from("/opt/homebrew/bin/llm-gateway"),
                unit_dir: dir.join("LaunchAgents"),
                log: dir.join("logs").join("supervise.log"),
                uid: 501,
                environment: vec![("XDG_STATE_HOME".to_owned(), dir.display().to_string())],
            },
        )
    }

    /// 載せるのは「書いてから叩く」。順序が逆だと、まだ無いファイルを指す。
    #[test]
    fn registering_writes_the_unit_and_then_tells_the_system() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        let runner = Recorder::default();

        register(&plan, &runner, false).unwrap();

        let written = std::fs::read_to_string(&plan.unit_path).unwrap();
        assert_eq!(written, plan.unit_text);
        assert_eq!(
            runner.calls(),
            vec![format!(
                "launchctl bootstrap gui/501 {}",
                plan.unit_path.display()
            )]
        );
    }

    /// `--dry-run` は何も触らない。移行の前に「何が起きるか」だけを見る。
    #[test]
    fn a_dry_run_touches_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        let runner = Recorder::default();

        register(&plan, &runner, true).unwrap();

        assert!(!plan.unit_path.exists());
        assert!(runner.calls().is_empty());
    }

    /// 二重に載せない。既にあるものを黙って上書きすると、載っているものと
    /// ファイルの中身がずれる。
    #[test]
    fn registering_twice_is_refused_by_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        register(&plan, &Recorder::default(), false).unwrap();

        let e = register(&plan, &Recorder::default(), false).unwrap_err();
        assert_eq!(e.kind(), "already_registered");
    }

    /// 載せられなかったら、書いたファイルも残さない。
    #[test]
    fn a_failed_bootstrap_leaves_no_unit_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());

        let e = register(&plan, &Refuses, false).unwrap_err();
        assert_eq!(e.kind(), "command_failed");
        assert!(e.message().contains("Bootstrap failed"), "{e:?}");
        assert!(!plan.unit_path.exists());
    }

    /// 降ろすのは「叩いてから消す」。消してから断られると、載ったままの
    /// ものを指すファイルが無くなる。
    #[test]
    fn unregistering_tells_the_system_before_removing_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        register(&plan, &Recorder::default(), false).unwrap();

        let refused = unregister(&plan, &Refuses).unwrap_err();
        assert_eq!(refused.kind(), "command_failed");
        assert!(plan.unit_path.exists(), "the file was removed anyway");

        let runner = Recorder::default();
        unregister(&plan, &runner).unwrap();
        assert_eq!(
            runner.calls(),
            vec!["launchctl bootout gui/501/jp.kawaz.llm-gateway.supervise"]
        );
        assert!(!plan.unit_path.exists());
    }

    /// 載っていないものは、始めも止めも降ろしもしない。
    #[test]
    fn nothing_is_asked_of_a_service_that_is_not_registered() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());

        for kind in [
            unregister(&plan, &Recorder::default())
                .unwrap_err()
                .kind()
                .to_owned(),
            start(&plan, &Recorder::default())
                .unwrap_err()
                .kind()
                .to_owned(),
            stop(&plan, &Recorder::default())
                .unwrap_err()
                .kind()
                .to_owned(),
        ] {
            assert_eq!(kind, "not_registered");
        }
    }

    #[test]
    fn starting_kickstarts_the_registered_label() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        register(&plan, &Recorder::default(), false).unwrap();

        let runner = Recorder::default();
        start(&plan, &runner).unwrap();
        assert_eq!(
            runner.calls(),
            vec!["launchctl kickstart gui/501/jp.kawaz.llm-gateway.supervise"]
        );
    }

    /// 監督者が居なければ、止めた瞬間に終わっている。
    #[test]
    fn stopping_something_that_holds_nothing_returns_at_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        register(&plan, &Recorder::default(), false).unwrap();

        let runner = Recorder::default();
        stop(&plan, &runner).unwrap();
        assert_eq!(
            runner.calls(),
            vec!["launchctl kill SIGTERM gui/501/jp.kawaz.llm-gateway.supervise"]
        );
    }

    /// 掴んだ繋がりが切れたら、畳み終わったということ。
    #[test]
    fn the_wait_ends_when_the_supervisor_lets_go_of_its_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("supervisor.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        // 監督者の代わり。1 本受けたら、そのまま終わる (= 待ち受けも繋がりも落ちる)。
        let supervisor = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        assert!(wait_while_stopping(&socket, || Ok(())).unwrap());
        supervisor.join().unwrap();
    }

    /// 3 つの問いは、それぞれ別の相手が答える。
    #[test]
    fn the_status_separates_registration_from_running_from_units() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        register(&plan, &Recorder::default(), false).unwrap();

        let runner = Recorder::saying(Output {
            code: 0,
            stdout: "\tstate = running\n\tpid = 4242\n\tlast exit code = 0\n".to_owned(),
            stderr: String::new(),
        });
        status(&plan, &runner, Some(json!({"units": [{"unit": "stable"}]}))).unwrap();
        assert_eq!(
            runner.calls(),
            vec!["launchctl print gui/501/jp.kawaz.llm-gateway.supervise"]
        );
    }

    /// launchctl の言い分から、載っている / 動いている / pid を読む。
    #[test]
    fn what_launchctl_says_becomes_the_service_part() {
        let out = Output {
            code: 0,
            stdout: "\tstate = running\n\tpid = 4242\n\tlast exit code = 0\n".to_owned(),
            stderr: String::new(),
        };
        assert_eq!(
            service_status(Kind::Launchd, &out),
            json!({"loaded": true, "running": true, "pid": 4242, "last_exit": "0"})
        );

        // 知らない label は非 0 で断られる = 載っていない。
        let unknown = Output {
            code: 113,
            stdout: String::new(),
            stderr: "Could not find service".to_owned(),
        };
        assert_eq!(
            service_status(Kind::Launchd, &unknown),
            json!({"loaded": false, "running": false})
        );
    }

    /// systemd の言い分も、同じ形に畳む。
    #[test]
    fn what_systemctl_says_becomes_the_same_shape() {
        let out = Output {
            code: 0,
            stdout: "LoadState=loaded\nActiveState=active\nMainPID=99\nExecMainStatus=0\n"
                .to_owned(),
            stderr: String::new(),
        };
        assert_eq!(
            service_status(Kind::Systemd, &out),
            json!({"loaded": true, "running": true, "pid": 99, "last_exit": "0"})
        );

        // 止まっている unit の MainPID は 0。誰の pid でもないので出さない。
        let stopped = Output {
            code: 0,
            stdout: "LoadState=loaded\nActiveState=inactive\nMainPID=0\nExecMainStatus=0\n"
                .to_owned(),
            stderr: String::new(),
        };
        assert_eq!(
            service_status(Kind::Systemd, &stopped),
            json!({"loaded": true, "running": false, "last_exit": "0"})
        );
    }

    /// 監督者が書く先は、載せる前に作っておく (無いと OS 側が起動に失敗する)。
    #[test]
    fn the_log_directory_is_made_before_the_service_is_loaded() {
        let dir = tempfile::TempDir::new().unwrap();
        let plan = plan_in(dir.path());
        register(&plan, &Recorder::default(), false).unwrap();
        assert!(dir.path().join("logs").is_dir());
    }

    /// 1 度も上がっていない監督者にログが無いのは、異常ではない。
    #[test]
    fn a_log_that_was_never_written_is_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            log(&dir.path().join("nothing.log"), false).unwrap(),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn a_written_log_is_printed_as_it_stands() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("supervise.log");
        std::fs::write(&path, "supervising\n").unwrap();
        assert_eq!(log(&path, false).unwrap(), ExitCode::SUCCESS);
    }

    /// 受け付けるのは名指しした旗だけ。読めない指定を黙って捨てない。
    #[test]
    fn only_the_flag_that_belongs_to_the_command_is_taken() {
        assert!(flag(&args(&["--dry-run"]), "dry-run").unwrap());
        assert!(!flag(&[], "dry-run").unwrap());
        assert!(flag(&args(&["--follow"]), "dry-run").is_err());
        assert!(no_options(&args(&["--all"])).is_err());
    }

    /// 受け付ける命令と、help に並ぶ命令は同じ (cli-design-preferences)。
    #[test]
    fn every_command_it_takes_is_written_in_the_help() {
        for command in ["register", "unregister", "start", "stop", "status", "log"] {
            assert!(
                help::SERVICE.contains(&format!("  {command} "))
                    || help::SERVICE.contains(&format!("  {command}\n")),
                "the help does not offer `{command}`"
            );
        }
        // 旗も同じ (打てるのに書いていない、を作らない)。
        assert!(help::SERVICE.contains("--dry-run"), "{}", help::SERVICE);
        assert!(help::SERVICE.contains("--follow"), "{}", help::SERVICE);

        let e = dispatch(&args(&["reload"])).unwrap_err();
        assert!(e.message().contains("there is no"), "{e:?}");
    }

    #[test]
    fn an_unknown_subcommand_points_at_the_level_help() {
        let e = dispatch(&args(&["registr"])).unwrap_err();
        assert!(e.message().contains("service --help"), "{e:?}");
    }

    #[test]
    fn the_level_shows_its_help_when_asked_for_nothing() {
        assert_eq!(dispatch(&[]).unwrap(), ExitCode::SUCCESS);
    }
}
