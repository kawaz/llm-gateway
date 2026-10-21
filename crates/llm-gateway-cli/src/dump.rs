//! 走っている台に、本文の一時ダンプを頼む (DR-0038)。
//!
//! 台を上げ下げも読み直しもしないので `daemon` の下には置かない (決定 3)。
//! 指し方は `usage` / `stats` と同じ `--unit` で、経路だけが HTTP でなく監督者の
//! socket になる (決定 2)。CLI は頼んで答えを出すだけで、何も書かない。

use std::process::ExitCode;

use llm_gateway::daemon::protocol::{self, DumpRelay, DumpRequest, Request};

use crate::destination;
use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};

pub fn dispatch(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::DUMP);
        return Ok(ExitCode::SUCCESS);
    }
    let rest = &args[1..];
    let parsed = match args[0].as_str() {
        "start" => start(rest)?,
        "stop" => stop(rest)?,
        "status" => status(rest)?,
        other => {
            return Err(Failure::from(format!(
                "there is no `dump {other}` command. see `llm-gateway dump --help`"
            )));
        }
    };
    let Some((unit, request)) = parsed else {
        // 必須の引数が無ければ help (決定 3)。
        print!("{}", help::DUMP);
        return Ok(ExitCode::SUCCESS);
    };
    let target = destination::resolve(&llm_gateway::daemon::registry::open(), unit.as_deref())?;
    ask(target.unit, request)
}

/// 頼みの形と、指された台 (無ければ登録簿で決める)。`None` は help を出す時。
type Parsed = Option<(Option<String>, DumpRequest)>;

fn start(args: &[String]) -> Result<Parsed, Failure> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut ns = None;
    let mut duration = None;
    let mut max_bytes = None;
    let mut request_only = false;
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("ns", inline)) => ns = Some(take_value("ns", inline, &mut it)?),
            Some(("for", inline)) => duration = Some(take_value("for", inline, &mut it)?),
            Some(("max-bytes", inline)) => {
                let raw = take_value("max-bytes", inline, &mut it)?;
                max_bytes = Some(raw.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                    Failure::from(format!(
                        "--max-bytes takes a whole number of bytes larger than zero, got `{raw}`"
                    ))
                })?);
            }
            Some(("request-only", None)) => request_only = true,
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    let ns = ns.ok_or_else(|| {
        Failure::from("--ns is required: a dump covers one namespace (see `dump --help`)")
    })?;
    let duration = duration.ok_or_else(|| {
        Failure::from("--for is required: a dump always ends by itself, like --for 10m")
    })?;
    let for_secs = llm_gateway::config::parse_duration_secs(&duration)
        .map_err(|e| Failure::from(format!("--for: {e}")))?;
    Ok(Some((
        unit,
        DumpRequest::Start {
            ns,
            for_secs: u64::try_from(for_secs).unwrap_or(0),
            max_bytes,
            request_only,
        },
    )))
}

fn stop(args: &[String]) -> Result<Parsed, Failure> {
    let mut dump_id = None;
    let mut all = false;
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("all", None)) => all = true,
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => {
                if dump_id.replace(arg.clone()).is_some() {
                    return Err(Failure::from(
                        "two dump ids were given. say one, or --all".to_owned(),
                    ));
                }
            }
        }
    }
    if dump_id.is_some() && all {
        return Err(Failure::from(
            "a dump id and --all were both given. say one of them".to_owned(),
        ));
    }
    if dump_id.is_none() && !all {
        return Ok(None);
    }
    Ok(Some((unit, DumpRequest::Stop { dump_id, all })))
}

fn status(args: &[String]) -> Result<Parsed, Failure> {
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    Ok(Some((unit, DumpRequest::Status)))
}

/// 監督者に頼んで、台の答えをそのまま出す。`ok: false` なら stderr へ回して非 0。
fn ask(unit: String, dump: DumpRequest) -> Result<ExitCode, Failure> {
    let socket = protocol::socket_path();
    let request = Request::Dump(DumpRelay { unit, dump });
    let answer = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?
        .block_on(async move {
            protocol::ask(&socket, &request)
                .await
                .map_err(|e| crate::daemon::control::not_running(&e))
        })?;
    print(&answer)
}

fn print(answer: &str) -> Result<ExitCode, Failure> {
    let value: serde_json::Value = serde_json::from_str(answer)
        .map_err(|e| Failure::from(format!("could not read the answer: {e}")))?;
    if value["ok"] != serde_json::json!(true) {
        eprintln!("{answer}");
        return Ok(ExitCode::FAILURE);
    }
    println!("{answer}");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// `start` は `--ns` と `--for` を読み、`--for` は秒にして渡す。
    #[test]
    fn start_reads_its_options_in_any_order() {
        let (unit, request) = start(&args(&[
            "--for=10m",
            "--ns",
            "lab",
            "--unit",
            "unstable",
            "--max-bytes",
            "1024",
            "--request-only",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(unit.as_deref(), Some("unstable"));
        assert_eq!(
            request,
            DumpRequest::Start {
                ns: "lab".to_owned(),
                for_secs: 600,
                max_bytes: Some(1024),
                request_only: true,
            }
        );
    }

    /// `--for` は必須で既定値を持たない。`--ns` も必須 (決定 1・4)。
    #[test]
    fn start_needs_a_namespace_and_a_duration() {
        assert_eq!(
            start(&args(&[])).unwrap(),
            None,
            "no arguments shows the help"
        );
        let e = start(&args(&["--ns", "lab"])).unwrap_err();
        assert!(e.message().contains("--for is required"), "{e:?}");
        let e = start(&args(&["--for", "10m"])).unwrap_err();
        assert!(e.message().contains("--ns is required"), "{e:?}");
        assert!(start(&args(&["--ns", "lab", "--for", "soon"])).is_err());
        assert!(start(&args(&["--ns", "lab", "--for", "1m", "--max-bytes", "0"])).is_err());
        assert!(start(&args(&["--ns", "lab", "--for", "1m", "--nope"])).is_err());
    }

    /// `stop` は id か `--all` のどちらか。どちらも無ければ help、両方は断る。
    #[test]
    fn stop_takes_an_id_or_all() {
        assert_eq!(
            stop(&args(&["019a", "--unit=u"])).unwrap(),
            Some((
                Some("u".to_owned()),
                DumpRequest::Stop {
                    dump_id: Some("019a".to_owned()),
                    all: false
                }
            ))
        );
        assert_eq!(
            stop(&args(&["--all"])).unwrap(),
            Some((
                None,
                DumpRequest::Stop {
                    dump_id: None,
                    all: true
                }
            ))
        );
        assert_eq!(stop(&args(&[])).unwrap(), None);
        assert!(stop(&args(&["a", "--all"])).is_err());
        assert!(stop(&args(&["a", "b"])).is_err());
    }

    /// `status` は省略できる引数だけなので、引数なしで実行する。
    #[test]
    fn status_runs_without_arguments() {
        assert_eq!(
            status(&args(&[])).unwrap(),
            Some((None, DumpRequest::Status))
        );
        assert!(status(&args(&["extra"])).is_err());
    }

    /// 受け付ける命令と、help に並ぶ命令は同じ (cli-design-preferences)。
    #[test]
    fn every_command_it_takes_is_written_in_the_help() {
        for command in ["start", "stop", "status"] {
            assert!(
                help::DUMP.contains(&format!("  {command} ")),
                "the help does not offer `{command}`"
            );
        }
        for option in [
            "--ns <ns>",
            "--for <duration>",
            "--max-bytes <n>",
            "--request-only",
            "--all",
            "--unit <name>",
        ] {
            assert!(help::DUMP.contains(option), "the help misses {option}");
        }
        let e = dispatch(&args(&["pause"])).unwrap_err();
        assert!(e.message().contains("dump --help"), "{e:?}");
    }

    /// 子を持つレベルと必須の引数を持つ命令は、引数なしで help を出す。
    #[test]
    fn the_level_and_its_required_arguments_show_the_help() {
        for level in [vec![], vec!["--help"], vec!["start"], vec!["stop"]] {
            assert_eq!(
                dispatch(&args(&level)).unwrap(),
                ExitCode::SUCCESS,
                "{level:?}"
            );
        }
    }

    /// 台が断った答えは stderr に回り、exit が非 0 になる。
    #[test]
    fn a_refusal_comes_back_as_a_failure() {
        assert_eq!(
            print(r#"{"unit":"u","ok":true,"dumps":[]}"#).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            print(r#"{"unit":"u","ok":false,"error":{"kind":"unknown_dump","message":"x"}}"#)
                .unwrap(),
            ExitCode::FAILURE
        );
        assert!(print("not json").is_err());
    }
}
