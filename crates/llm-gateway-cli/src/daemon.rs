//! この端末で走らせる台の操作 (DR-0028 決定 1〜4)。
//!
//! 登録簿を触るのは `add` / `remove` / `list`、実際に走らせるのは `run`。
//! `start` / `stop` / `restart` / `status` は監督者への要求で、監督者が居ない
//! なら断る — 代わりに自分で起こしたりはしない (所有者が 2 つになる)。

pub mod control;
pub mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use llm_gateway::daemon::registry::{self, Registry, Unit};
use serde_json::json;

use crate::failure::Failure;
use crate::help;
use crate::options::{split, take_value};

pub fn dispatch(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::DAEMON);
        return Ok(ExitCode::SUCCESS);
    }

    let rest = &args[1..];
    match args[0].as_str() {
        "run" => run::foreground(&Registry::open(), rest),
        "add" => add(&Registry::open(), rest),
        "remove" => remove(&Registry::open(), rest),
        "list" => list(&Registry::open(), control::running_now()),
        "supervise" => control::supervise(rest),
        op @ ("start" | "stop" | "restart" | "status") => control::ask(op, rest),
        "log" => control::log(rest),
        other => Err(Failure::from(format!(
            "there is no `daemon {other}` command. see `llm-gateway daemon --help`"
        ))),
    }
}

/// 設定ファイルを 1 台として登録する。
///
/// 登録するだけで起こさない。上げ下げは `daemon start` / 監督者の仕事で、
/// 「足したら勝手に動いていた」を作らない。
fn add(registry: &Registry, args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() {
        print!("{}", help::DAEMON);
        return Ok(ExitCode::SUCCESS);
    }
    let (config, name) = parse_add(args)?;

    // 設定として読めることを、登録の前に確かめる。読めない設定を登録すると、
    // 走らせようとした時点で初めて分かる。
    let loaded = crate::load(&config)?;
    let config = absolute(&config);
    let name = match name {
        Some(name) => name,
        None => registry::name_from_config(&config)?,
    };
    // 設定に書いてあればそれ、無ければ「今の自分」を焼く。ただし今の自分が
    // `target/debug` のような消えるパスなら、同じ binary を指す PATH 上の
    // 安定な場所を選ぶ (焼いたパスが消えると、監督者は上げ直せない)。
    let mut warning = None;
    let binary_path = match loaded.server.binary_path {
        Some(binary) => absolute(&binary),
        None => {
            let me = std::env::current_exe()
                .map_err(|e| Failure::from(format!("could not find my own path: {e}")))?;
            let resolved = crate::executable::resolve(&me, None)?;
            if let Some(said) = &resolved.warning {
                eprintln!("llm-gateway daemon add: warning: {said}");
            }
            warning = resolved.warning;
            resolved.path
        }
    };

    let unit = Unit {
        config,
        binary_path,
        enabled: true,
        added_at: llm_gateway::credential::time::format_rfc3339(
            llm_gateway::credential::time::now_unix(),
        ),
    };
    registry.add(&name, &unit)?;
    let mut row = entry(&name, &unit);
    if let Some(warning) = warning {
        row.insert("warning".to_owned(), json!(warning));
    }
    println!("{}", json!(row));
    Ok(ExitCode::SUCCESS)
}

fn parse_add(args: &[String]) -> Result<(PathBuf, Option<String>), Failure> {
    let mut config: Option<PathBuf> = None;
    let mut name: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("name", inline)) => name = Some(take_value("name", inline, &mut it)?),
            Some(("config", inline)) => {
                config = Some(PathBuf::from(take_value("config", inline, &mut it)?));
            }
            Some(_) => return Err(Failure::from(format!("could not understand `{arg}`"))),
            None => {
                if let Some(previous) = config.replace(PathBuf::from(arg)) {
                    return Err(Failure::from(format!(
                        "two configuration files were given (`{}` and `{arg}`). \
one unit is one configuration file",
                        previous.display()
                    )));
                }
            }
        }
    }

    let config = config.ok_or_else(|| {
        Failure::from(
            "no configuration file was given. \
give it as `llm-gateway daemon add <config>`",
        )
    })?;
    Ok((config, name))
}

/// 登録簿から外す。走っている台を止めるのは監督者の仕事。
fn remove(registry: &Registry, args: &[String]) -> Result<ExitCode, Failure> {
    let Some(name) = args.first() else {
        print!("{}", help::DAEMON);
        return Ok(ExitCode::SUCCESS);
    };
    registry.remove(name)?;
    println!("{}", json!({"unit": name, "removed": true}));
    Ok(ExitCode::SUCCESS)
}

/// 登録されている台を並べる。
///
/// 動いているかを言えるのは監督者に聞けたときだけ。聞けなければ登録簿の
/// 中身だけを出す — `list` は登録を見る命令なので、監督者が居ないことを
/// 理由に断らない (DR-0028 決定 3)。登録簿を読んだだけで「動いている」と
/// 書くこともしない (知らないので)。
fn list(registry: &Registry, running: Option<serde_json::Value>) -> Result<ExitCode, Failure> {
    let units: Vec<_> = registry
        .list()?
        .iter()
        .enumerate()
        .map(|(i, (name, unit))| {
            let mut row = entry(name, unit);
            row.insert("id".to_owned(), json!(i));
            if let Some(status) = status_of(running.as_ref(), name) {
                row.insert("running".to_owned(), json!(status["running"]));
                if let Some(pid) = status.get("pid") {
                    row.insert("pid".to_owned(), pid.clone());
                }
            }
            row
        })
        .collect();
    println!("{}", json!(units));
    Ok(ExitCode::SUCCESS)
}

/// 監督者が答えた行から、1 台ぶんを探す。
fn status_of<'a>(
    running: Option<&'a serde_json::Value>,
    name: &str,
) -> Option<&'a serde_json::Value> {
    running?
        .get("units")?
        .as_array()?
        .iter()
        .find(|row| row.get("unit").and_then(|u| u.as_str()) == Some(name))
}

/// 1 台ぶんの JSON。
fn entry(name: &str, unit: &Unit) -> serde_json::Map<String, serde_json::Value> {
    let mut row = serde_json::Map::new();
    row.insert("unit".to_owned(), json!(name));
    row.insert("enabled".to_owned(), json!(unit.enabled));
    row.insert("config".to_owned(), json!(unit.config));
    row.insert("binary_path".to_owned(), json!(unit.binary_path));
    row
}

/// 相対パスのまま焼き込むと、監督者の cwd 次第で別のファイルを指す。
fn absolute(path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 設定ファイルを書いて、その場所を返す。
    fn config_file(dir: &std::path::Path, name: &str, listen: &str) -> PathBuf {
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, format!("[server]\nlisten = \"{listen}\"\n")).unwrap();
        path
    }

    #[test]
    fn add_takes_a_configuration_file_and_an_optional_name() {
        let (config, name) = parse_add(&args(&["/tmp/c.toml"])).unwrap();
        assert_eq!(config, PathBuf::from("/tmp/c.toml"));
        assert_eq!(name, None);

        let (_, name) = parse_add(&args(&["/tmp/c.toml", "--name", "unstable"])).unwrap();
        assert_eq!(name.as_deref(), Some("unstable"));

        // オプションはメイン引数の前でも後ろでもよい。
        let (config, name) = parse_add(&args(&["--name=stable", "/tmp/c.toml"])).unwrap();
        assert_eq!(config, PathBuf::from("/tmp/c.toml"));
        assert_eq!(name.as_deref(), Some("stable"));
    }

    /// 設定ファイルが 2 つ来たら、どちらを登録するのか分からない。
    #[test]
    fn add_refuses_two_configuration_files() {
        let e = parse_add(&args(&["/tmp/a.toml", "/tmp/b.toml"])).unwrap_err();
        assert!(
            e.message().contains("one unit is one configuration"),
            "{e:?}"
        );
        assert!(parse_add(&args(&[])).is_err(), "a file is required");
        assert!(parse_add(&args(&["--nope"])).is_err());
    }

    /// 名前を省いたら設定ファイル名から付き、実行ファイルは自分自身になる。
    #[test]
    fn a_unit_gets_its_name_and_binary_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        let config = config_file(dir.path(), "config-11301-unstable", "127.0.0.1:11301");

        add(&registry, &args(&[config.to_str().unwrap()])).unwrap();

        let unit = registry.get("config-11301-unstable").unwrap();
        assert_eq!(unit.config, config);
        assert_eq!(unit.binary_path, std::env::current_exe().unwrap());
        assert!(unit.enabled, "a freshly added unit is wanted");
        assert!(!unit.added_at.is_empty());
    }

    /// 設定に実行ファイルが書いてあれば、そちらを焼き込む
    /// (面ごとに別のビルドを走らせる運用がある)。
    #[test]
    fn the_configured_binary_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        let config = dir.path().join("stable.toml");
        std::fs::write(
            &config,
            "[server]\nlisten = \"127.0.0.1:11302\"\nbinary_path = \"/opt/homebrew/bin/llm-gateway\"\n",
        )
        .unwrap();

        add(
            &registry,
            &args(&[config.to_str().unwrap(), "--name", "stable"]),
        )
        .unwrap();

        assert_eq!(
            registry.get("stable").unwrap().binary_path,
            PathBuf::from("/opt/homebrew/bin/llm-gateway")
        );
    }

    /// 読めない設定は登録しない。走らせようとした時点で初めて気づくのを避ける。
    #[test]
    fn an_unreadable_configuration_is_not_registered() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        let config = dir.path().join("broken.toml");
        std::fs::write(&config, "[server]\nlisten = 11301\n").unwrap();

        assert!(add(&registry, &args(&[config.to_str().unwrap()])).is_err());
        assert!(registry.names().is_empty());
    }

    /// 相対パスのまま焼き込むと、監督者の cwd 次第で別のファイルを指す。
    #[test]
    fn a_relative_path_is_stored_as_an_absolute_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        let config = config_file(dir.path(), "rel", "127.0.0.1:11301");

        // 相対で渡しても、絶対に直って入る。
        let relative = pathdiff(&config);
        add(&registry, &args(&[&relative, "--name", "rel"])).unwrap();

        assert!(registry.get("rel").unwrap().config.is_absolute());
    }

    /// cwd からの相対表記を作る (作れない環境ではそのまま絶対を返す)。
    fn pathdiff(path: &std::path::Path) -> String {
        let cwd = std::env::current_dir().unwrap();
        path.strip_prefix(&cwd)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }

    #[test]
    fn a_unit_can_be_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        let config = config_file(dir.path(), "a", "127.0.0.1:11301");
        add(&registry, &args(&[config.to_str().unwrap()])).unwrap();

        remove(&registry, &args(&["a"])).unwrap();
        assert!(registry.names().is_empty());
        assert_eq!(
            remove(&registry, &args(&["a"])).unwrap_err().kind(),
            "unknown_unit"
        );
    }

    /// 登録簿の行には、走っているかどうかを書かない (知らないので)。
    #[test]
    fn a_listed_unit_says_nothing_about_whether_it_runs() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        let config = config_file(dir.path(), "a", "127.0.0.1:11301");
        add(&registry, &args(&[config.to_str().unwrap()])).unwrap();

        let row = entry("a", &registry.get("a").unwrap());
        assert_eq!(
            row.keys().collect::<Vec<_>>(),
            vec!["unit", "enabled", "config", "binary_path"]
        );
        assert!(list(&registry, None).is_ok());
    }

    /// 監督者に聞けたときだけ、行に「今いるか」が足される。
    #[test]
    fn what_is_running_comes_from_the_supervisor() {
        let answer = json!({"units": [{"unit": "a", "running": true, "pid": 42}]});
        let row = status_of(Some(&answer), "a").unwrap();
        assert_eq!(row["pid"], json!(42));
        // 聞けていない / 知らない台については、何も足さない。
        assert!(status_of(Some(&answer), "b").is_none());
        assert!(status_of(None, "a").is_none());
    }

    /// 受け付ける命令と、help に並ぶ命令は同じ (cli-design-preferences)。
    #[test]
    fn every_command_it_takes_is_written_in_the_help() {
        for command in [
            "run",
            "supervise",
            "add",
            "remove",
            "list",
            "start",
            "stop",
            "restart",
            "status",
            "log",
        ] {
            assert!(
                help::DAEMON.contains(&format!("  {command} "))
                    || help::DAEMON.contains(&format!("  {command}\n")),
                "the help does not offer `{command}`"
            );
        }
        // 逆向き: help に無いものは受け付けない。
        let e = dispatch(&args(&["reload"])).unwrap_err();
        assert!(e.message().contains("there is no"), "{e:?}");
    }

    #[test]
    fn an_unknown_subcommand_points_at_the_level_help() {
        let e = dispatch(&args(&["lst"])).unwrap_err();
        assert!(e.message().contains("daemon --help"), "{e:?}");
    }

    /// 子を持つレベルは、引数なしで help を出す。
    #[test]
    fn the_level_shows_its_help_when_asked_for_nothing() {
        assert_eq!(dispatch(&[]).unwrap(), ExitCode::SUCCESS);
        assert_eq!(dispatch(&args(&["--help"])).unwrap(), ExitCode::SUCCESS);
    }
}
