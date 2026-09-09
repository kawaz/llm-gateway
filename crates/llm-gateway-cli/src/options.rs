//! オプションの読み方で共通するところ。
//!
//! `--key value` と `--key=value` はどちらでも書ける。オプションはメイン引数の
//! 後ろにも置ける (cli-design-preferences)。

use std::path::PathBuf;

use crate::failure::Failure;

/// `--key value` / `--key=value` のどちらからでも値を取る。
pub fn take_value(
    key: &str,
    inline: Option<&str>,
    it: &mut std::slice::Iter<'_, String>,
) -> Result<String, Failure> {
    match inline {
        Some(value) => Ok(value.to_owned()),
        None => it
            .next()
            .cloned()
            .ok_or_else(|| Failure::from(format!("--{key} needs a value"))),
    }
}

/// `--<key>` を `(key, inline)` に割る。オプションでなければ `None`。
pub fn split(arg: &str) -> Option<(&str, Option<&str>)> {
    let rest = arg.strip_prefix("--")?;
    Some(match rest.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (rest, None),
    })
}

/// `--config <path>` だけを受ける命令のための読み取り。
///
/// 省略時は既定の設定ファイル。
pub fn config_path(args: &[String]) -> Result<PathBuf, Failure> {
    let mut path: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match split(arg) {
            Some(("config", inline)) => {
                path = Some(PathBuf::from(take_value("config", inline, &mut it)?));
            }
            _ => return Err(Failure::from(format!("could not understand `{arg}`"))),
        }
    }
    Ok(path.unwrap_or_else(llm_gateway::Config::default_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_config_path_is_read_either_way() {
        assert_eq!(
            config_path(&args(&["--config", "/tmp/c.toml"])).unwrap(),
            PathBuf::from("/tmp/c.toml")
        );
        assert_eq!(
            config_path(&args(&["--config=/tmp/c.toml"])).unwrap(),
            PathBuf::from("/tmp/c.toml")
        );
        assert_eq!(
            config_path(&args(&[])).unwrap(),
            llm_gateway::Config::default_path()
        );
    }

    /// 読めない指定は黙って既定に落とさない。別の設定を見たまま進む。
    #[test]
    fn an_unreadable_option_is_refused() {
        assert!(config_path(&args(&["--nope"])).is_err());
        assert!(config_path(&args(&["/tmp/c.toml"])).is_err());
        assert!(config_path(&args(&["--config"])).is_err());
    }
}
