//! llm-gateway のコマンドライン。
//!
//! 命令は責務ごとに分かれている (DR-0028):
//! - [`daemon`] この端末で走らせる台 — 登録簿と foreground の起動
//! - [`service`] OS への常駐登録 (監督者 1 つ)
//! - [`upstream`] upstream (Anthropic / OpenAI ...) が何と言っているか
//! - [`check`] / [`models`] 設定ファイルそのものを対象にする確認
//! - [`usage`] / [`stats`] 走っている台に聞いて整形する
//! - [`login`] ブラウザで認可を通して認証情報を置く

mod check;
mod daemon;
mod destination;
mod failure;
mod help;
mod login;
mod models;
mod options;
mod service;
mod stats;
mod text;
mod upstream;
mod usage;

use std::path::Path;
use std::process::ExitCode;

use llm_gateway::Config;

use crate::failure::Failure;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match run(&args) {
        Ok(code) => code,
        Err(failure) => {
            // エラーは JSON で stderr へ (DR-0028 決定 8)。読む側が `kind` で
            // 分岐でき、人は `message` を読めばよい。
            eprintln!("{}", failure.to_json());
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() {
        print!("{}", help::TOP);
        return Ok(ExitCode::SUCCESS);
    }
    if args.iter().any(|a| a == "--version") {
        println!("llm-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let command = args[0].as_str();
    let rest = &args[1..];

    // 子を持つレベルは、自分の階層の help を自分で出す。
    match command {
        "daemon" => return daemon::dispatch(rest),
        "service" => return service::dispatch(rest),
        "upstream" => return upstream::run(rest),
        _ => {}
    }

    if help::wanted(args) {
        print!("{}", help::TOP);
        return Ok(ExitCode::SUCCESS);
    }

    match command {
        "check" => check::run(&options::config_path(rest)?),
        "models" => models::run(&options::config_path(rest)?),
        "usage" => usage::run(rest),
        "stats" => stats::run(rest),
        "login" => login::run(rest),
        other => Err(Failure::from(format!(
            "there is no `{other}` command. see `llm-gateway --help`"
        ))),
    }
}

/// 設定ファイルを読む。
fn load(path: &Path) -> Result<Config, Failure> {
    Config::load(path).map_err(|e| Failure::from(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// 無い名前は help へ案内する。
    #[test]
    fn unknown_command_points_at_help() {
        let e = run(&args(&["logon"])).unwrap_err();
        assert!(e.message().contains("--help"), "{e:?}");
    }

    /// 消した語は復活しない。`serve` も裸の `status` も、もう無い。
    #[test]
    fn the_removed_commands_are_gone() {
        for removed in ["serve", "status"] {
            let e = run(&args(&[removed])).unwrap_err();
            assert!(
                e.message().contains(&format!("there is no `{removed}`")),
                "{removed}: {e:?}"
            );
        }
    }

    /// 引数なしは help。何も起こさない。
    #[test]
    fn no_arguments_shows_the_top_help() {
        assert_eq!(run(&[]).unwrap(), ExitCode::SUCCESS);
    }

    /// 階層ごとに違う help が出る。
    #[test]
    fn each_level_answers_its_own_help() {
        for level in [
            vec!["--help"],
            vec!["daemon", "--help"],
            vec!["service", "--help"],
            vec!["upstream", "--help"],
            vec!["daemon"],
            vec!["service"],
            vec!["upstream"],
        ] {
            assert_eq!(run(&args(&level)).unwrap(), ExitCode::SUCCESS, "{level:?}");
        }
    }

    #[test]
    fn the_version_is_printed_on_request() {
        assert_eq!(run(&args(&["--version"])).unwrap(), ExitCode::SUCCESS);
    }
}
