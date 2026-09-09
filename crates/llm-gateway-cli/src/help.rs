//! 各レベルの help (DR-0028 決定 8: help だけはテキスト)。
//!
//! 子を持つレベルと必須の引数があるコマンドは、引数なしでもここを出す。

/// 一番上。
pub const TOP: &str = "\
llm-gateway — a thin LLM proxy that keeps authentication out of the client

usage:
  llm-gateway <command> [options]
  llm-gateway login --type <type> <name>

commands:
  daemon      run the gateway processes of this installation
  service     put the supervisor on (or take it off) the operating system
  upstream    show what the upstream services report
  check       read a configuration and verify it (without starting)
  models      list the models written in the configuration
  usage       list usage per credential (asks a running unit)
  stats       list token usage and USD cost per credential x model x day
  login       authorize in a browser and save the credential to <name>.json

global options:
  --help, -h        show the help of the level it is given at
  --version         show the version

check options:
  --config <path>   configuration file to read
                    (default: $XDG_CONFIG_HOME/llm-gateway/config.toml)

models options:
  --config <path>   configuration file to read

usage options:
  --unit <name>     which running unit to ask
                    (default: the only registered one; `daemon list` shows them)
  --refresh         also send a minimal request to idle credentials to read them again
                    (the check itself consumes a little usage)

stats options:
  --unit <name>     which running unit to ask
  --days <N>        show the last N days (default: 7, 0 for everything)

login options:
  --type <type>     claude_oauth or codex_oauth
                    (the same word as the type written in [credentials.<name>] of config.toml)
  --config <path>   configuration file that declares the credential

environment variables:
  LLM_GATEWAY_LOG   log verbosity (default: info)
  XDG_CONFIG_HOME   default location for the configuration
  XDG_STATE_HOME    default location for credentials, the unit registry, and logs
";

/// `daemon` の下。
pub const DAEMON: &str = "\
llm-gateway daemon — run the gateway processes of this installation

usage:
  llm-gateway daemon <command> [options]

a unit is one configuration file, registered under a name. `run` starts one in
the foreground; `start` / `stop` / `restart` / `status` ask the supervisor, and
say so if it is not running.

commands:
  run <unit>              run one unit in the foreground
  supervise               run the supervisor in the foreground (it holds the units)
  add <config>            register a configuration file as a unit
  remove <unit>           drop a unit from the registry
  list                    list the registered units
  start <unit>|--all      ask the supervisor to start it
  stop <unit>|--all       ask the supervisor to stop it
  restart <unit>|--all    ask the supervisor to restart it (one at a time)
  status [<unit>]|--all   ask the supervisor how they are doing
  log [<unit>]|--all      show what they wrote

add options:
  --name <name>     name of the unit
                    (default: the configuration file name without its extension)

log options:
  --follow          keep printing as more is written

output:
  results are JSON on stdout, errors are JSON on stderr, and this help is text
";

/// `service` の下。
pub const SERVICE: &str = "\
llm-gateway service — put the supervisor on (or take it off) the operating system

usage:
  llm-gateway service <command> [options]

what is registered is the supervisor alone (`llm-gateway daemon supervise`).
which units it holds is the registry's business, not the operating system's.

commands:
  register          register the supervisor (launchd on macOS, systemd --user on Linux)
  unregister        take it off again
  start             start the registered supervisor
  stop              stop it
  status            show whether it is registered and running
  log               show what it wrote

log options:
  --follow          keep printing as more is written
";

/// `upstream` の下。
pub const UPSTREAM: &str = "\
llm-gateway upstream — show what the upstream services report

usage:
  llm-gateway upstream <command> [options]

commands:
  status            show configured upstream service status (asks a running unit)

status options:
  --unit <name>     which running unit to ask
                    (default: the only registered one; `daemon list` shows them)
  --refresh         refresh official sources before showing the report
";

/// help を求められているか。
pub fn wanted(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn help_is_asked_for_by_either_spelling() {
        assert!(wanted(&args(&["--help"])));
        assert!(wanted(&args(&["list", "-h"])));
        assert!(!wanted(&args(&["list"])));
    }

    /// 上の help は、実際に受け付ける命令を全部並べる。
    #[test]
    fn the_top_help_lists_every_command() {
        for command in [
            "daemon", "service", "upstream", "check", "models", "usage", "stats", "login",
        ] {
            assert!(TOP.contains(command), "missing {command}");
        }
    }

    /// 消した語を案内し続けない (`serve` / 単独の `status` は無い)。
    #[test]
    fn the_top_help_does_not_offer_what_was_removed() {
        assert!(!TOP.contains("  serve"), "{TOP}");
        assert!(!TOP.contains("\n  status"), "{TOP}");
    }

    #[test]
    fn each_level_lists_its_own_commands() {
        for command in [
            "run",
            "supervise",
            "add",
            "remove",
            "list",
            "start",
            "restart",
        ] {
            assert!(DAEMON.contains(command), "daemon help misses {command}");
        }
        for command in ["register", "unregister", "status", "log"] {
            assert!(SERVICE.contains(command), "service help misses {command}");
        }
        assert!(UPSTREAM.contains("status"), "{UPSTREAM}");
    }

    /// 宛先の指し方は、聞きに行く命令すべてに書いてある。
    #[test]
    fn the_commands_that_ask_a_unit_document_how_to_point_at_one() {
        assert!(UPSTREAM.contains("--unit"), "{UPSTREAM}");
        // usage / stats は上の階層に住んでいる。
        assert_eq!(TOP.matches("--unit <name>").count(), 2, "{TOP}");
    }
}
