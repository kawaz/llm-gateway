//! 監督者に頼む口 (DR-0028 決定 3)。
//!
//! `start` / `stop` / `restart` / `status` は、自分では何も動かさない。
//! socket 越しに監督者へ渡し、返ってきたものをそのまま出す。監督者が
//! 居なければ断る — 代わりに子を起こすと、止める相手が誰なのか分からなくなる。

use std::process::ExitCode;
use std::sync::Arc;

use llm_gateway::daemon::protocol::{self, Request, Which};
use llm_gateway::daemon::supervisor::Supervisor;

use crate::failure::Failure;
use crate::options::split;

/// この端末の前で監督する。止められるまで戻らない。
pub fn supervise(args: &[String]) -> Result<ExitCode, Failure> {
    if let Some(unexpected) = args.first() {
        return Err(Failure::from(format!(
            "could not understand `{unexpected}`"
        )));
    }
    init_logging();

    let supervisor = Arc::new(Supervisor::open());
    runtime()?
        .block_on(async move { supervisor.supervise().await })
        .map_err(Failure::from)?;
    Ok(ExitCode::SUCCESS)
}

/// 監督者に頼んで、答えをそのまま出す。
pub fn ask(op: &str, args: &[String]) -> Result<ExitCode, Failure> {
    // 数えるだけの命令は、指されなければ全部でよい。動かす命令は指させる。
    let counting = op == "status";
    let which = target(args, counting)?;
    let request = match op {
        "start" => Request::Start(which),
        "stop" => Request::Stop(which),
        "restart" => Request::Restart(which),
        "status" => Request::Status(which),
        other => return Err(Failure::from(format!("there is no `daemon {other}`"))),
    };

    let socket = protocol::socket_path();
    let answer = runtime()?.block_on(async move {
        protocol::ask(&socket, &request)
            .await
            .map_err(|e| not_running(&e))
    })?;
    print(&answer)
}

/// 子が書いたものを出す。`--follow` なら書かれ続けるものも。
pub fn log(args: &[String]) -> Result<ExitCode, Failure> {
    let mut rest = Vec::new();
    let mut follow = false;
    for arg in args {
        match split(arg) {
            Some(("follow", None)) => follow = true,
            _ => rest.push(arg.clone()),
        }
    }
    let which = target(&rest, true)?;

    if !follow {
        return dump(&which);
    }

    let socket = protocol::socket_path();
    runtime()?.block_on(async move {
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|e| not_running(&e))?;
        let mut lines = protocol::send(stream, &Request::Log(which))
            .await
            .map_err(|e| not_running(&e))?;

        let mut code = ExitCode::SUCCESS;
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| Failure::from(format!("the supervisor stopped talking: {e}")))?
        {
            // 断られたのなら、追従は始まっていない。
            if line.contains("\"error\"")
                && serde_json::from_str::<serde_json::Value>(&line)
                    .is_ok_and(|value| value.get("error").is_some())
            {
                eprintln!("{line}");
                code = ExitCode::FAILURE;
                break;
            }
            println!("{line}");
        }
        Ok(code)
    })
}

/// 監督者を通さずに、置いてあるログを読む。
///
/// 追い続けないぶんだけ、監督者が居なくても答えられる。書いたものは
/// 監督者の持ち物ではなくファイルなので、居ないことを理由に断らない。
fn dump(which: &Which) -> Result<ExitCode, Failure> {
    let registry = llm_gateway::daemon::registry::Registry::open();
    let names = which
        .choose(&registry.names(), true)
        .map_err(|message| Failure::new("unit_required", message))?;

    let dir = protocol::log_dir();
    let many = names.len() > 1;
    for name in &names {
        let path = protocol::log_path(&dir, name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // まだ 1 度も走っていない台にログが無いのは、異常ではない。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Failure::from(format!(
                    "could not read {}: {e}",
                    path.display()
                )));
            }
        };
        for line in text.lines() {
            if many {
                println!("[{name}] {line}");
            } else {
                println!("{line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// 監督者が居れば、登録簿の行に「今いるか」を足す。
///
/// 居なければ登録簿だけを出す。`list` は登録を見る命令なので、監督者が
/// 居ないことを理由に断らない (DR-0028 決定 3)。
pub fn running_now() -> Option<serde_json::Value> {
    let socket = protocol::socket_path();
    if !socket.exists() {
        return None;
    }
    let runtime = tokio::runtime::Runtime::new().ok()?;
    let answer = runtime
        .block_on(async move { protocol::ask(&socket, &Request::Status(Which::all())).await })
        .ok()?;
    serde_json::from_str::<serde_json::Value>(&answer).ok()
}

/// 名前 / `--all` を読む。
fn target(args: &[String], bare_is_all: bool) -> Result<Which, Failure> {
    let mut which = Which::default();
    for arg in args {
        match split(arg) {
            Some(("all", None)) => which.all = true,
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => {
                if which.unit.replace(arg.clone()).is_some() {
                    return Err(Failure::from(
                        "two units were given. say one name, or --all".to_owned(),
                    ));
                }
            }
        }
    }
    if which.unit.is_some() && which.all {
        return Err(Failure::from(
            "a name and --all were both given. say one of them".to_owned(),
        ));
    }
    if which.unit.is_none() && !which.all && !bare_is_all {
        return Err(Failure::new("unit_required", "say which unit, or --all"));
    }
    Ok(which)
}

/// 監督者の答えを出す。断られていれば stderr へ回して非 0 で終わる。
fn print(answer: &str) -> Result<ExitCode, Failure> {
    let value: serde_json::Value = serde_json::from_str(answer)
        .map_err(|e| Failure::from(format!("could not read the answer: {e}")))?;
    if value.get("error").is_some() {
        eprintln!("{answer}");
        return Ok(ExitCode::FAILURE);
    }
    println!("{answer}");
    Ok(ExitCode::SUCCESS)
}

/// 監督者に繋げなかった。何をすれば繋がるのかまで言う。
fn not_running(reason: &dyn std::fmt::Display) -> Failure {
    Failure::new(
        "supervisor_not_running",
        format!(
            "could not reach the supervisor at {} ({reason})",
            protocol::socket_path().display()
        ),
    )
    .with(
        "hint",
        "run `llm-gateway daemon supervise` or `llm-gateway service start`",
    )
}

fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter =
        EnvFilter::try_from_env("LLM_GATEWAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 名前でも `--all` でも指せる。
    #[test]
    fn a_unit_is_pointed_at_by_name_or_by_all() {
        assert_eq!(
            target(&args(&["stable"]), false).unwrap(),
            Which::named("stable")
        );
        assert_eq!(target(&args(&["--all"]), false).unwrap(), Which::all());
    }

    /// 動かす命令は、何も指されなければ動かさない。
    #[test]
    fn moving_something_needs_a_target() {
        let e = target(&args(&[]), false).unwrap_err();
        assert_eq!(e.kind(), "unit_required");
        // 数えるだけの命令は、指されなければ全部。
        assert_eq!(target(&args(&[]), true).unwrap(), Which::default());
    }

    /// 曖昧な指し方は断る (どちらのつもりか分からないまま動かさない)。
    #[test]
    fn an_ambiguous_target_is_refused() {
        assert!(target(&args(&["a", "--all"]), false).is_err());
        assert!(target(&args(&["a", "b"]), false).is_err());
        assert!(target(&args(&["--nope"]), false).is_err());
    }

    /// 断られた答えは stderr に回り、exit が非 0 になる。
    #[test]
    fn a_refusal_comes_back_as_a_failure() {
        assert_eq!(print(r#"{"units":[]}"#).unwrap(), ExitCode::SUCCESS);
        assert_eq!(
            print(r#"{"error":{"kind":"unknown_unit"}}"#).unwrap(),
            ExitCode::FAILURE
        );
        assert!(print("not json").is_err());
    }

    /// 繋がらないときは、繋げる方法まで言う。
    #[test]
    fn an_absent_supervisor_says_how_to_start_one() {
        let failure = not_running(&"No such file or directory");
        assert_eq!(failure.kind(), "supervisor_not_running");
        let value: serde_json::Value = serde_json::from_str(&failure.to_json()).unwrap();
        assert!(
            value["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("daemon supervise"),
            "{value}"
        );
    }
}
