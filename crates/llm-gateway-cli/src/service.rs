//! OS への常駐登録 (DR-0028 決定 7)。
//!
//! 登録するのは監督者 1 つだけ。どの台を抱えるかは登録簿の話で、OS は知らない。

use std::process::ExitCode;

use crate::failure::{Failure, not_implemented};
use crate::help;

pub fn dispatch(args: &[String]) -> Result<ExitCode, Failure> {
    if args.is_empty() || help::wanted(args) {
        print!("{}", help::SERVICE);
        return Ok(ExitCode::SUCCESS);
    }
    match args[0].as_str() {
        command @ ("register" | "unregister" | "start" | "stop" | "status" | "log") => {
            Err(not_implemented(&format!("service {command}")))
        }
        other => Err(Failure::from(format!(
            "there is no `service {other}` command. see `llm-gateway service --help`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// help には並ぶが、まだ動かないことを名指しで言う。
    #[test]
    fn every_service_command_says_it_is_not_there_yet() {
        for command in ["register", "unregister", "start", "stop", "status", "log"] {
            let e = dispatch(&args(&[command])).unwrap_err();
            assert_eq!(e.kind(), "not_implemented", "{command}");
            assert!(e.message().contains(command), "{command}: {e:?}");
        }
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
