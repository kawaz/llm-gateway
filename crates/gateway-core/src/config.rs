//! 設定の読み込み基盤。
//!
//! 何を設定するかは利用側が決める。ここが持つのは、設定ファイルを土台に
//! 重ねる規則 ([`extends`])、パスとして書かれた値の開き方 ([`path_expand`])、
//! 既定の置き場の決め方だけ。

use std::path::PathBuf;

pub mod extends;
pub mod path_expand;

/// 消えると作り直せないものの置き場。`app` はその下の 1 段 (製品の名前)。
pub fn default_state_dir(app: &str) -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join(app)
}

/// XDG の環境変数が指す場所。無い / 相対パスなら `$HOME` の下の `fallback`。
pub fn xdg_dir(env: &str, fallback: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
