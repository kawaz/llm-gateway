//! 走っている台に、本文の一時ダンプを頼む (DR-0038)。
//!
//! 台を上げ下げも読み直しもしないので `daemon` の下には置かない (決定 1)。
//! 指し方は `usage` / `stats` と同じ `--unit` で、経路だけが HTTP でなく監督者の
//! socket になる。CLI は頼んで答えを出すだけで、何も書かない。

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
        "list" => list(rest)?,
        "prune" => prune(rest)?,
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
    // 頼む先を決められない・監督者に届かない時も、台の答えと同じ
    // `{unit, ok: false, error}` の形で断る (決定 2)。
    let target = match destination::resolve(&llm_gateway::daemon::registry::open(), unit.as_deref())
    {
        Ok(target) => target,
        Err(failure) => return Ok(refuse(unit.as_deref(), &failure.to_json())),
    };
    if matches!(request, DumpRequest::Start { follow: true, .. }) {
        return follow(&target.unit, request);
    }
    match ask(&target.unit, request) {
        Ok(answer) => print(&target.unit, &answer),
        Err(failure) => Ok(refuse(Some(&target.unit), &failure.to_json())),
    }
}

/// `start --follow`: 台が流す行をそのまま stdout へ出し続ける (DR-0038 決定 12)。
///
/// 系列が止まると台は `stop` の行の後で閉じ、ここも終わる。Ctrl-C でこの
/// プロセスが終わると繋がりが閉じ、台は系列を `disconnected` で止める。
/// 断られた時 (`ok: false` の 1 行) は stderr へ出して非 0。
fn follow(unit: &str, dump: DumpRequest) -> Result<ExitCode, Failure> {
    let socket = protocol::socket_path();
    let request = Request::DumpFollow(DumpRelay {
        unit: unit.to_owned(),
        dump,
    });
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?;
    let streamed = runtime.block_on(async move {
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|e| crate::daemon::control::not_running(&e))?;
        let mut lines = protocol::send(stream, &request)
            .await
            .map_err(|e| crate::daemon::control::not_running(&e))?;
        let mut code = ExitCode::SUCCESS;
        while let Ok(Some(line)) = lines.next_line().await {
            let refused = serde_json::from_str::<serde_json::Value>(&line).is_ok_and(|value| {
                value.get("ok") == Some(&serde_json::json!(false))
                    || value.get("error").is_some() && value.get("event").is_none()
            });
            if refused {
                eprintln!("{}", refusal(Some(unit), &line));
                code = ExitCode::FAILURE;
                break;
            }
            println!("{line}");
        }
        Ok::<ExitCode, Failure>(code)
    });
    match streamed {
        Ok(code) => Ok(code),
        Err(failure) => Ok(refuse(Some(unit), &failure.to_json())),
    }
}

/// `{"error": …}` の形の断りを `{unit, ok: false, error}` に直して stderr へ出す。
fn refuse(unit: Option<&str>, refused: &str) -> ExitCode {
    eprintln!("{}", refusal(unit, refused));
    ExitCode::FAILURE
}

fn refusal(unit: Option<&str>, refusal: &str) -> serde_json::Value {
    let error = serde_json::from_str::<serde_json::Value>(refusal)
        .ok()
        .and_then(|value| value.get("error").cloned())
        .unwrap_or_else(|| serde_json::json!({"kind": "bad_answer", "message": refusal}));
    let mut answer = serde_json::Map::new();
    if let Some(unit) = unit {
        answer.insert("unit".to_owned(), serde_json::json!(unit));
    }
    answer.insert("ok".to_owned(), serde_json::json!(false));
    answer.insert("error".to_owned(), error);
    serde_json::Value::Object(answer)
}

/// 頼みの形と、指された台 (無ければ登録簿で決める)。`None` は help を出す時。
type Parsed = Option<(Option<String>, DumpRequest)>;

fn start(args: &[String]) -> Result<Parsed, Failure> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut ns = None;
    let mut timeout_ms = None;
    let mut max_bytes = None;
    let mut store = None;
    let mut labels = std::collections::BTreeMap::new();
    let mut follow = false;
    let mut embed = None;
    let mut persist = None;
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("ns", inline)) => ns = Some(take_value("ns", inline, &mut it)?),
            Some(("timeout", inline)) => {
                let raw = take_value("timeout", inline, &mut it)?;
                let secs = llm_gateway::config::parse_duration_secs(&raw)
                    .map_err(|e| Failure::from(format!("--timeout: {e}")))?;
                timeout_ms = Some(u64::try_from(secs).unwrap_or(0).saturating_mul(1000));
            }
            Some(("max-bytes", inline)) => {
                let raw = take_value("max-bytes", inline, &mut it)?;
                max_bytes = Some(raw.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                    Failure::from(format!(
                        "--max-bytes takes a whole number of bytes larger than zero, got `{raw}`"
                    ))
                })?);
            }
            Some(("store", inline)) => store = Some(take_value("store", inline, &mut it)?),
            Some(("label", inline)) => {
                let raw = take_value("label", inline, &mut it)?;
                let (key, value) = raw.split_once('=').ok_or_else(|| {
                    Failure::from(format!("--label takes key=value, got `{raw}`"))
                })?;
                labels.insert(key.to_owned(), value.to_owned());
            }
            Some(("follow", None)) => follow = true,
            Some(("embed", inline)) => embed = Some(take_value("embed", inline, &mut it)?),
            Some(("no-persist", None)) => persist = Some(false),
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    let Some(ns) = ns else {
        // 必須の --ns が無ければ help (決定 1)。
        return Ok(None);
    };
    Ok(Some((
        unit,
        DumpRequest::Start {
            ns,
            timeout_ms,
            max_bytes,
            store,
            labels,
            follow,
            embed,
            persist,
        },
    )))
}

fn stop(args: &[String]) -> Result<Parsed, Failure> {
    let mut dump_id = None;
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => {
                if dump_id.replace(arg.clone()).is_some() {
                    return Err(Failure::from(
                        "two dump ids were given. stop takes one".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(dump_id.map(|dump_id| (unit, DumpRequest::Stop { dump_id })))
}

fn list(args: &[String]) -> Result<Parsed, Failure> {
    let mut selectors = Vec::new();
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => selectors.push(arg.clone()),
        }
    }
    Ok(Some((unit, DumpRequest::List { selectors })))
}

fn prune(args: &[String]) -> Result<Parsed, Failure> {
    let mut dump_ids = Vec::new();
    let mut selectors = Vec::new();
    let mut unit = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("dump-id", inline)) => dump_ids.push(take_value("dump-id", inline, &mut it)?),
            Some(("unit", inline)) => unit = Some(take_value("unit", inline, &mut it)?),
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => selectors.push(arg.clone()),
        }
    }
    if dump_ids.is_empty() && selectors.is_empty() {
        // 無指定の全消しはしない。何も指さなければ help。
        return Ok(None);
    }
    Ok(Some((
        unit,
        DumpRequest::Prune {
            dump_ids,
            selectors,
        },
    )))
}

/// 監督者に頼んで、答えの 1 行を受け取る。
fn ask(unit: &str, dump: DumpRequest) -> Result<String, Failure> {
    let socket = protocol::socket_path();
    let request = Request::Dump(DumpRelay {
        unit: unit.to_owned(),
        dump,
    });
    tokio::runtime::Runtime::new()
        .map_err(|e| Failure::from(format!("could not start the async runtime: {e}")))?
        .block_on(async move {
            protocol::ask(&socket, &request)
                .await
                .map_err(|e| crate::daemon::control::not_running(&e))
        })
}

/// 台の答えをそのまま出す。`ok: false` なら stderr へ回して非 0。
///
/// `ok` を持たない答えは、dump を知らない版の監督者が `{"error": …}` で
/// 断ったもの。台の答えと同じ形に直して出す。
fn print(unit: &str, answer: &str) -> Result<ExitCode, Failure> {
    let value: serde_json::Value = serde_json::from_str(answer)
        .map_err(|e| Failure::from(format!("could not read the answer: {e}")))?;
    match value.get("ok") {
        Some(serde_json::Value::Bool(true)) => {
            println!("{answer}");
            Ok(ExitCode::SUCCESS)
        }
        Some(_) => {
            eprintln!("{answer}");
            Ok(ExitCode::FAILURE)
        }
        None => Ok(refuse(Some(unit), answer)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// `start` は `--ns` ほかを読み、`--timeout` はミリ秒にして渡す。
    #[test]
    fn start_reads_its_options_in_any_order() {
        let (unit, request) = start(&args(&[
            "--timeout=10m",
            "--ns",
            "lab",
            "--unit",
            "unstable",
            "--max-bytes",
            "1024",
            "--label",
            "owner=kawaz",
            "--label=memo=pr1234",
            "--store",
            "raw",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(unit.as_deref(), Some("unstable"));
        assert_eq!(
            request,
            DumpRequest::Start {
                ns: "lab".to_owned(),
                timeout_ms: Some(600_000),
                max_bytes: Some(1024),
                store: Some("raw".to_owned()),
                labels: std::collections::BTreeMap::from([
                    ("memo".to_owned(), "pr1234".to_owned()),
                    ("owner".to_owned(), "kawaz".to_owned()),
                ]),
                follow: false,
                embed: None,
                persist: None,
            }
        );
    }

    /// `--ns` は必須で、無ければ help。`--timeout` は省ける (台の既定)。
    #[test]
    fn start_needs_a_namespace() {
        assert_eq!(
            start(&args(&[])).unwrap(),
            None,
            "no arguments shows the help"
        );
        assert_eq!(start(&args(&["--timeout", "10m"])).unwrap(), None);
        let (_, request) = start(&args(&["--ns", "lab"])).unwrap().unwrap();
        assert!(matches!(
            request,
            DumpRequest::Start {
                timeout_ms: None,
                ..
            }
        ));
        assert!(start(&args(&["--ns", "lab", "--timeout", "soon"])).is_err());
        assert!(start(&args(&["--ns", "lab", "--max-bytes", "0"])).is_err());
        assert!(start(&args(&["--ns", "lab", "--label", "novalue"])).is_err());
        assert!(start(&args(&["--ns", "lab", "--nope"])).is_err());
    }

    /// `stop` は id を 1 つ取る。無ければ help。
    #[test]
    fn stop_takes_one_id() {
        assert_eq!(
            stop(&args(&["019a", "--unit=u"])).unwrap(),
            Some((
                Some("u".to_owned()),
                DumpRequest::Stop {
                    dump_id: "019a".to_owned()
                }
            ))
        );
        assert_eq!(stop(&args(&[])).unwrap(), None);
        assert!(stop(&args(&["a", "b"])).is_err());
        assert!(stop(&args(&["a", "--all"])).is_err());
    }

    /// `list` は選択子を並べて取り、無くても実行する。
    #[test]
    fn list_takes_selectors_and_runs_without_any() {
        assert_eq!(
            list(&args(&[])).unwrap(),
            Some((None, DumpRequest::List { selectors: vec![] }))
        );
        assert_eq!(
            list(&args(&["state=active", "owner"])).unwrap(),
            Some((
                None,
                DumpRequest::List {
                    selectors: vec!["state=active".to_owned(), "owner".to_owned()]
                }
            ))
        );
    }

    /// `prune` は `--dump-id` か選択子のどちらかが要り、無ければ help。
    #[test]
    fn prune_needs_an_id_or_a_selector() {
        assert_eq!(prune(&args(&[])).unwrap(), None);
        assert_eq!(
            prune(&args(&["--dump-id", "a", "--dump-id=b"])).unwrap(),
            Some((
                None,
                DumpRequest::Prune {
                    dump_ids: vec!["a".to_owned(), "b".to_owned()],
                    selectors: vec![]
                }
            ))
        );
        assert!(matches!(
            prune(&args(&["owner=kawaz"])).unwrap(),
            Some((_, DumpRequest::Prune { .. }))
        ));
    }

    /// 受け付ける命令と、help に並ぶ命令は同じ (cli-design-preferences)。
    #[test]
    fn every_command_it_takes_is_written_in_the_help() {
        for command in ["start", "stop", "list", "prune"] {
            assert!(
                help::DUMP.contains(&format!("  {command} ")),
                "the help does not offer `{command}`"
            );
        }
        for option in [
            "--ns <ns>",
            "--timeout <duration>",
            "--max-bytes <n>",
            "--store raw",
            "--label <key>=<value>",
            "--follow",
            "--embed bodies",
            "--no-persist",
            "--dump-id <id>",
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
        for level in [
            vec![],
            vec!["--help"],
            vec!["start"],
            vec!["stop"],
            vec!["prune"],
        ] {
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
            print("u", r#"{"unit":"u","ok":true,"dumps":[]}"#).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            print(
                "u",
                r#"{"unit":"u","ok":false,"error":{"kind":"unknown_dump","message":"x"}}"#
            )
            .unwrap(),
            ExitCode::FAILURE
        );
        // dump を知らない監督者の断り (`ok` が無い) も失敗。
        assert_eq!(
            print("u", r#"{"error":{"kind":"bad_request","message":"x"}}"#).unwrap(),
            ExitCode::FAILURE
        );
        assert!(print("u", "not json").is_err());
    }

    /// 頼む先を決められない時も `{unit, ok: false, error}` の形で、`error` は
    /// 元の断りの欄 (kind / message / 添えた欄) をそのまま持つ (決定 2)。
    #[test]
    fn a_refusal_before_reaching_the_unit_has_the_same_shape() {
        let failure = Failure::new("unit_required", "say which").with("units", vec!["a", "b"]);
        assert_eq!(refuse(None, &failure.to_json()), ExitCode::FAILURE);
        assert_eq!(
            refuse(
                Some("u"),
                r#"{"error":{"kind":"supervisor_not_running","message":"x"}}"#
            ),
            ExitCode::FAILURE
        );
    }
}
