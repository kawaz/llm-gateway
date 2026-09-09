//! この端末で走らせる台の操作 (DR-0028 決定 1〜4)。
//!
//! 登録簿を触るのは `add` / `remove` / `list`、実際に走らせるのは `run`。
//! `start` / `stop` / `restart` / `status` は監督者への要求で、監督者が居ない
//! なら断る — 代わりに自分で起こしたりはしない (所有者が 2 つになる)。

pub mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use llm_gateway::daemon::registry::{self, Registry, Unit};
use serde_json::json;

use crate::failure::{Failure, not_implemented};
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
        "list" => list(&Registry::open()),
        // 監督者まわりは段階を分けて入れる。help には並ぶが、まだ動かない。
        command @ ("supervise" | "start" | "stop" | "restart" | "status" | "log") => {
            Err(not_implemented(&format!("daemon {command}")))
        }
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
    let binary_path = match loaded.server.binary_path {
        Some(binary) => absolute(&binary),
        None => std::env::current_exe()
            .map_err(|e| Failure::from(format!("could not find my own path: {e}")))?,
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
    println!("{}", json!(entry(&name, &unit)));
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
/// 動いているかはここでは言わない。それを知っているのは監督者だけで、
/// 登録簿を読んだだけで「動いている」と書くと嘘になる。
fn list(registry: &Registry) -> Result<ExitCode, Failure> {
    let units: Vec<_> = registry
        .list()?
        .iter()
        .enumerate()
        .map(|(i, (name, unit))| {
            let mut row = entry(name, unit);
            row.insert("id".to_owned(), json!(i));
            row
        })
        .collect();
    println!("{}", json!(units));
    Ok(ExitCode::SUCCESS)
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
        assert!(list(&registry).is_ok());
    }

    /// 監督者に頼む口は、まだ無いことを名指しで言う。
    #[test]
    fn the_supervisor_commands_say_they_are_not_there_yet() {
        for command in ["supervise", "start", "stop", "restart", "status", "log"] {
            let e = dispatch(&args(&[command])).unwrap_err();
            assert_eq!(e.kind(), "not_implemented", "{command}");
            assert!(e.message().contains(command), "{command}: {e:?}");
        }
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
