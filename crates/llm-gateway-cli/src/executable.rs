//! 焼き込む実行ファイルのパスを決める。
//!
//! `service register` は OS の登録に、`daemon add` は登録簿に、それぞれ binary の
//! パスを書き込む。どちらも「今の自分 (`current_exe`)」をそのまま焼くと、
//! `target/debug` や `Cellar/<version>` のような**次のビルド・次の upgrade で
//! 消えるパス**が残る。同じ binary を指す PATH 上の symlink
//! (`/opt/homebrew/bin/llm-gateway` など) があるなら、そちらを焼く。
//!
//! 判定は stable-which に任せる (hyoui DR-0031 / cache-warden DR-0019 §2.5 と
//! 同じ方針)。安定なパスが無くても止めない — 開発中は `target/release` を
//! 走らせるのが正しいので、断らずに警告だけ添える。

use std::path::{Path, PathBuf};

use stable_which::{ScoringPolicy, resolve_stable_path};

use crate::failure::Failure;

/// 焼き込むパスと、それが安定でないときの一言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub warning: Option<String>,
}

/// どのパスを焼くか決める。
///
/// `explicit` (`--executable <path>`) が与えられていれば、存在だけ確かめて
/// そのまま使う。安定かどうかは渡した人の責任なので、警告も付けない。
pub fn resolve(current_exe: &Path, explicit: Option<&str>) -> Result<Resolved, Failure> {
    if let Some(given) = explicit {
        let path = PathBuf::from(given);
        if !path.exists() {
            return Err(Failure::new(
                "no_such_executable",
                format!("--executable {given} does not exist"),
            ));
        }
        return Ok(Resolved {
            path,
            warning: None,
        });
    }

    let candidate = resolve_stable_path(current_exe, ScoringPolicy::SameBinary).map_err(|e| {
        Failure::new(
            "unresolvable_executable",
            format!(
                "could not resolve a stable path for {}: {e}",
                current_exe.display()
            ),
        )
    })?;
    Ok(decide(candidate.path(), candidate.is_stable()))
}

/// 選んだパスに、安定でないときの一言を添える。
///
/// 「安定でない」は stable-which の判定 (dev build の出力、一時の場所、
/// 版が入った install 先、見覚えのない場所) をそのまま使う。
fn decide(path: &Path, stable: bool) -> Resolved {
    Resolved {
        path: path.to_path_buf(),
        warning: (!stable).then(|| {
            format!(
                "no durable install path was found; baking {} in as it is \
(a dev build, a versioned install, or an unrecognized location — it can break on the \
next build or upgrade). Install it on PATH, or pass `--executable <path>`.",
                path.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 安定なパスなら、そのまま黙って焼く。
    #[test]
    fn a_durable_path_is_taken_without_a_word() {
        assert_eq!(
            decide(Path::new("/opt/homebrew/bin/llm-gateway"), true),
            Resolved {
                path: PathBuf::from("/opt/homebrew/bin/llm-gateway"),
                warning: None,
            }
        );
    }

    /// 安定でなくても止めない。断ると開発中のビルドを常駐させられなくなる。
    #[test]
    fn an_unstable_path_is_still_taken_but_named() {
        let resolved = decide(Path::new("/repo/target/release/llm-gateway"), false);
        assert_eq!(
            resolved.path,
            PathBuf::from("/repo/target/release/llm-gateway")
        );
        let warning = resolved.warning.unwrap();
        assert!(
            warning.contains("/repo/target/release/llm-gateway"),
            "{warning}"
        );
        assert!(warning.contains("--executable"), "{warning}");
    }

    /// 試験の binary は `target/debug/deps/...` にある dev build なので、
    /// 実際に解決させると警告が付く道を通る。
    #[test]
    fn a_dev_build_resolves_to_itself_with_a_warning() {
        let me = std::env::current_exe().unwrap();
        let resolved = resolve(&me, None).unwrap();
        assert!(resolved.warning.is_some(), "{resolved:?}");
    }

    /// 明示されたパスは、安定かどうかを問わずそのまま使う。
    #[test]
    fn an_explicit_path_is_used_as_it_stands() {
        let me = std::env::current_exe().unwrap();
        let resolved = resolve(&me, Some(me.to_str().unwrap())).unwrap();
        assert_eq!(resolved.path, me);
        assert_eq!(resolved.warning, None);
    }

    /// 無いパスを焼くと、起動できない常駐が登録される。
    #[test]
    fn an_explicit_path_that_is_not_there_is_refused() {
        let e = resolve(Path::new("/bin/sh"), Some("/nowhere/llm-gateway")).unwrap_err();
        assert_eq!(e.kind(), "no_such_executable");
    }
}
