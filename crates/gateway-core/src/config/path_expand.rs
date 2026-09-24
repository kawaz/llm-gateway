//! 設定に書いたパスの `~` と環境変数を、読み込むときに 1 回だけ開く。
//!
//! 設定ファイルは人の手で書かれ、dotfiles に置かれて台をまたぐ。そこに
//! `/Users/<誰か>/...` が焼き込まれていると、台を変えた瞬間に壊れる。
//! 既定値の側は最初から `$XDG_STATE_HOME` と `~` で組み立てているので
//! (`xdg_dir`)、**書いた側でも同じ書き方が通る**ようにする。
//!
//! 開く形は 4 つだけ:
//!
//! | 書き方 | 意味 |
//! |---|---|
//! | 先頭の `~` / `~/` | `$HOME` |
//! | `$VAR` | その環境変数 |
//! | `${VAR}` | 同上 (後ろに文字が続く場合) |
//! | `${VAR:-代わり}` | 無ければ「代わり」(こちらも同じ規則で開く) |
//!
//! 定まらない `$VAR` は **名指しで落とす**。空文字として通すと、`~/.local`
//! のつもりが `/.local` を指したまま動き、消えたはずの場所に書き込む。
//!
//! `\$` の逃がし方は用意しない。パスに literal な `$` を書く場面が無く、
//! 逃がし文字を足すと「`\` はいつ必要か」を覚える側の負担だけが残る。

use std::path::PathBuf;

use crate::{Error, Result};

/// 開くのに失敗した理由。どの欄かを添えるのは呼んだ側 (serde が欄名を付ける)。
type Failed<T> = std::result::Result<T, String>;

/// 環境の読み方。
///
/// Design rationale: 試験のために `std::env::set_var` を使うと、同じ実行
/// ファイルで並んで走る他の試験の `HOME` まで書き換わる (2024 edition で
/// `unsafe` になったのはこのため)。読み口を引数にして、試験は偽の環境を
/// 渡す。
trait Env {
    fn get(&self, name: &str) -> Option<String>;
}

/// 本番の環境。空文字は「定まっていない」と数える。
struct Real;

impl Env for Real {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var_os(name)
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
    }
}

/// パスとして書かれた 1 つの値を開く。
pub fn expand(raw: &str) -> Result<String> {
    expand_in(raw, &Real).map_err(Error::Config)
}

/// 開いた結果をパスとして受け取る。
///
/// 失敗は理由の文字列だけを返す。設定の欄から呼ばれたときは serde が
/// 「どの欄か」を添えるので、ここで「設定が読めない」と言い直すと同じ文が
/// 二重に出る。
pub fn expand_path(raw: &str) -> Failed<PathBuf> {
    expand_in(raw, &Real).map(PathBuf::from)
}

fn expand_in(raw: &str, env: &dyn Env) -> Failed<String> {
    let tilde_opened = open_leading_tilde(raw, env);
    open_vars(&tilde_opened, raw, env)
}

/// 先頭の `~` だけを開く。
///
/// 途中の `~` は開かない — `~` は「ここが誰かの家」を意味する記号で、
/// パスの途中に現れたら、それはただの文字。
fn open_leading_tilde(raw: &str, env: &dyn Env) -> String {
    let rest = if raw == "~" {
        ""
    } else if let Some(rest) = raw.strip_prefix("~/") {
        rest
    } else {
        return raw.to_owned();
    };

    let home = env.get("HOME").unwrap_or_else(|| "/".to_owned());
    if rest.is_empty() {
        home
    } else {
        format!("{}/{rest}", home.trim_end_matches('/'))
    }
}

/// `$VAR` / `${VAR}` / `${VAR:-代わり}` を開く。
///
/// `whole` は人が書いた元の値。落とすときに、どの欄を直せばよいかを言う。
fn open_vars(text: &str, whole: &str, env: &dyn Env) -> Failed<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];

        if let Some(body) = after.strip_prefix('{') {
            let (inside, tail) = split_at_close(body)
                .ok_or_else(|| format!("`{whole}` has a `${{` that is never closed with `}}`"))?;
            out.push_str(&open_braced(inside, whole, env)?);
            rest = tail;
        } else {
            let len = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            if len == 0 {
                return Err(format!(
                    "`{whole}` has a `$` that names nothing. Write `$VAR` or `${{VAR}}`"
                ));
            }
            out.push_str(&lookup(&after[..len], None, whole, env)?);
            rest = &after[len..];
        }
    }

    out.push_str(rest);
    Ok(out)
}

/// `${` の中身と、閉じ `}` の続きに切り分ける。
///
/// 中身に `${...}` が入れ子で書けるので (`${A:-${B}}`)、深さを数える。
fn split_at_close(body: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, c) in body.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return Some((&body[..i], &body[i + 1..])),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// `${...}` の中身を読む。
fn open_braced(inside: &str, whole: &str, env: &dyn Env) -> Failed<String> {
    match inside.split_once(":-") {
        Some((name, fallback)) => lookup(name, Some(fallback), whole, env),
        None => lookup(inside, None, whole, env),
    }
}

/// 環境変数を引く。無ければ「代わり」を同じ規則で開く。
fn lookup(name: &str, fallback: Option<&str>, whole: &str, env: &dyn Env) -> Failed<String> {
    if name.is_empty() {
        return Err(format!("`{whole}` has a `${{}}` with no variable name"));
    }
    if let Some(found) = env.get(name) {
        return Ok(found);
    }
    match fallback {
        // 「代わり」も人が書いた値なので、同じ規則で開く。
        // `${XDG_DATA_HOME:-~/.local/share}` の `~` はここで開く。
        Some(fallback) => expand_in(fallback, env),
        None => Err(format!(
            "`{whole}` uses the environment variable `{name}`, which is not set. \
             Set it, or write a fallback as `${{{name}:-...}}`"
        )),
    }
}

/// 設定の受け口。読み込んだ時点で開いた値にする。
pub mod serde_path {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn deserialize<'de, D>(de: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(de)?;
        super::expand_path(&raw).map_err(serde::de::Error::custom)
    }

    pub fn serialize<S>(path: &Path, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&path.to_string_lossy())
    }
}

/// 省略できるパスの受け口。
pub mod serde_opt_path {
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn deserialize<'de, D>(de: D) -> Result<Option<PathBuf>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(raw) = Option::<String>::deserialize(de)? else {
            return Ok(None);
        };
        super::expand_path(&raw)
            .map(Some)
            .map_err(serde::de::Error::custom)
    }

    pub fn serialize<S>(path: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match path {
            Some(path) => s.serialize_str(&path.to_string_lossy()),
            None => s.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 台の環境を装う。並んで走る他の試験に触らない。
    struct Fake(&'static [(&'static str, &'static str)]);

    impl Env for Fake {
        fn get(&self, name: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
                .filter(|v| !v.is_empty())
        }
    }

    const HOME: Fake = Fake(&[("HOME", "/home/someone")]);

    fn open(raw: &str, env: &Fake) -> String {
        expand_in(raw, env).unwrap()
    }

    fn fail(raw: &str, env: &Fake) -> String {
        expand_in(raw, env).unwrap_err().to_string()
    }

    /// 先頭の `~` は家になる。
    #[test]
    fn a_leading_tilde_becomes_home() {
        assert_eq!(open("~/.config/x", &HOME), "/home/someone/.config/x");
        assert_eq!(open("~", &HOME), "/home/someone");
    }

    /// 途中の `~` はただの文字。
    #[test]
    fn a_tilde_in_the_middle_is_just_a_character() {
        assert_eq!(open("/var/~/x", &HOME), "/var/~/x");
    }

    /// `$VAR` と `${VAR}` が開く。後者は後ろに文字が続く場合に要る。
    #[test]
    fn a_variable_opens_in_both_spellings() {
        let env = Fake(&[("ROOT", "/srv/root")]);
        assert_eq!(open("$ROOT/a", &env), "/srv/root/a");
        assert_eq!(open("${ROOT}bin", &env), "/srv/rootbin");
    }

    /// `$HOME` も普通の環境変数として開く。
    #[test]
    fn home_is_readable_as_a_variable() {
        assert_eq!(open("$HOME/.local", &HOME), "/home/someone/.local");
    }

    /// 既定値と同じ書き方が通る: 定まっていれば環境変数が勝つ。
    #[test]
    fn a_fallback_yields_to_the_variable_when_it_is_set() {
        let env = Fake(&[("HOME", "/home/someone"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(
            open("${XDG_DATA_HOME:-~/.local/share}/repos", &env),
            "/data/repos"
        );
    }

    /// 定まっていなければ「代わり」を使い、その `~` も開く。
    #[test]
    fn a_fallback_is_opened_by_the_same_rules() {
        assert_eq!(
            open("${XDG_DATA_HOME:-~/.local/share}/repos", &HOME),
            "/home/someone/.local/share/repos"
        );
    }

    /// 「代わり」の中にもう 1 段書ける。
    #[test]
    fn a_fallback_can_hold_another_variable() {
        let env = Fake(&[("INNER", "/inner")]);
        assert_eq!(open("${OUTER:-${INNER}/x}", &env), "/inner/x");
    }

    /// 代わりの無い未定義は、変数名を名指しで落とす。
    #[test]
    fn an_undefined_variable_is_named_in_the_error() {
        let e = fail("$XDG_DATA_HOME/x", &HOME);
        assert!(e.contains("XDG_DATA_HOME"), "{e}");
        assert!(e.contains("not set"), "{e}");
    }

    /// 空の環境変数は「定まっていない」と数える。
    ///
    /// 空を通すと `/x` のような根からのパスになり、気づかないまま別の場所へ
    /// 書き込む。
    #[test]
    fn an_empty_variable_counts_as_unset() {
        let env = Fake(&[("HOME", "/home/someone"), ("EMPTY", "")]);
        assert_eq!(open("${EMPTY:-~/fallback}", &env), "/home/someone/fallback");
        assert!(expand_in("$EMPTY/x", &env).is_err());
    }

    /// 閉じていない `${` は、その場で言う。
    #[test]
    fn an_unclosed_brace_is_rejected() {
        let e = fail("${HOME/x", &HOME);
        assert!(e.contains("never closed"), "{e}");
    }

    /// 名前の無い `$` も落とす。
    #[test]
    fn a_dollar_naming_nothing_is_rejected() {
        let e = fail("/tmp/$/x", &HOME);
        assert!(e.contains("names nothing"), "{e}");
    }

    /// 何も書いていない値は、そのまま通る。
    #[test]
    fn a_plain_path_is_left_alone() {
        let plain = "/opt/homebrew/bin/llm-gateway";
        assert_eq!(open(plain, &HOME), plain);
    }
}
