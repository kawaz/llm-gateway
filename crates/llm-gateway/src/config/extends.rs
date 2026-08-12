//! 設定を土台の上に重ねる (DR-0013)。
//!
//! `extends = "パス"` を書いたファイルは、そのパスの設定を**土台**にして、
//! 自分に書いた分で上書きする。同じ内容を 2 つのファイルに書き写して
//! 手で揃える運用 (待ち受け先だけ違う 2 台、など) を無くすため。
//!
//! 重ね方は 1 つだけ覚えればよい: **表 (テーブル) は鍵ごとに潜って重ね、
//! それ以外は丸ごと置き換える**。配列を要素ごとに混ぜないのは、並び自体が
//! 意味を持つため (経路の優先順) — 混ぜると、派生側が書いた順が土台の
//! 要素で崩れる。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::{Error, Result};

/// 土台をたどって 1 枚に畳んだ設定を返す。
///
/// `path` は読み込みの起点。相対パスの `extends` は、**それを書いたファイルの
/// 位置**から解く (呼び出し時の作業ディレクトリは見ない) — 設定ファイルの
/// 意味が、どこから起動したかで変わってはいけない。
pub fn resolve(path: &Path) -> Result<Table> {
    let mut visiting = BTreeSet::new();
    layer(path, None, &mut visiting)
}

/// 1 枚読んで、土台があればその上に重ねる。
///
/// `referrer` は「このファイルを指した側」。読めなかったときに、どのファイルの
/// どの行を直せばよいかを言うために持ち回る。
fn layer(path: &Path, referrer: Option<&Path>, visiting: &mut BTreeSet<PathBuf>) -> Result<Table> {
    // 同じファイルは実体で見分ける。`./a.toml` と `a.toml` と symlink 越しの
    // 同じファイルを別物と数えると、循環を見逃す。
    let here = path.canonicalize().map_err(|e| match referrer {
        Some(from) => Error::Config(format!(
            "could not read {} (extended by {}): {e}",
            path.display(),
            from.display()
        )),
        None => Error::Config(format!("could not read {}: {e}", path.display())),
    })?;

    if !visiting.insert(here.clone()) {
        return Err(Error::Config(format!(
            "config base is circular (returned to {}). Check the extends chain",
            here.display()
        )));
    }

    let raw = std::fs::read_to_string(&here)
        .map_err(|e| Error::Config(format!("could not read {}: {e}", here.display())))?;
    let mut table: Table = toml::from_str(&raw)
        .map_err(|e| Error::Config(format!("{} has invalid content: {e}", here.display())))?;

    // 取り出して消す。畳んだ結果に残すと、設定の項目として弾かれる。
    let Some(base) = table.remove("extends") else {
        return Ok(table);
    };
    let Some(base) = base.as_str() else {
        return Err(Error::Config(format!(
            "{}'s extends must be a file path (string)",
            here.display()
        )));
    };

    // 相対パスは、それを書いたファイルの隣から。
    let base = here.parent().unwrap_or(Path::new(".")).join(base);
    let mut merged = layer(&base, Some(&here), visiting)?;
    stack(&mut merged, table);
    Ok(merged)
}

/// 土台 (`base`) の上に `over` を重ねる。
///
/// 表は鍵ごとに潜り、それ以外 (数・文字列・真偽・時刻・配列) は丸ごと
/// 置き換える。**消す手段は用意しない** — 土台から項目を取り除きたいなら、
/// 土台を直すか、意味の無い値で塗り潰す。「どこかの派生ファイルが消している
/// かもしれない」を疑いながら設定を読む状態にしたくない。
fn stack(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(into)), Value::Table(from)) => stack(into, from),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 名前と中身の組をそのまま置く。
    fn spread(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
        dir
    }

    fn at(dir: &TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    const BASE: &str = r#"
[server]
listen = "127.0.0.1:8401"

[credentials.bedrock]
type = "bedrock_api_key"

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock.invalid/anthropic"
exclude = ["claude-opus-*"]

[[ns.default.routing]]
models = ["m"]
routes = ["bedrock"]
"#;

    /// 派生側に書いた分だけが変わり、書いていない分は土台のまま。
    #[test]
    fn what_is_not_written_comes_from_the_base() {
        let dir = spread(&[
            ("base.toml", BASE),
            (
                "here.toml",
                r#"
extends = "base.toml"

[server]
listen = "127.0.0.1:8402"
"#,
            ),
        ]);

        let merged = resolve(&at(&dir, "here.toml")).unwrap();
        assert_eq!(merged["server"]["listen"].as_str(), Some("127.0.0.1:8402"));
        assert_eq!(
            merged["routes"]["bedrock"]["url"].as_str(),
            Some("https://bedrock.invalid/anthropic"),
            "触っていない土台の項目は残る"
        );
        assert!(
            merged.get("extends").is_none(),
            "重ね終わったら、指示そのものは残さない"
        );
    }

    /// 表は鍵ごとに潜る。1 つ書き換えても、隣は土台のまま。
    #[test]
    fn tables_are_merged_key_by_key() {
        let dir = spread(&[
            ("base.toml", BASE),
            (
                "here.toml",
                r#"
extends = "base.toml"

[routes.bedrock]
exclude = ["*"]
"#,
            ),
        ]);

        let route = &resolve(&at(&dir, "here.toml")).unwrap()["routes"]["bedrock"];
        assert_eq!(
            route["provider"].as_str(),
            Some("anthropic"),
            "書いていない項目は土台から来る"
        );
        assert_eq!(
            route["url"].as_str(),
            Some("https://bedrock.invalid/anthropic")
        );
        assert_eq!(
            route["exclude"].as_array().unwrap(),
            &[Value::from("*")],
            "書いた項目だけが変わる"
        );
    }

    /// 配列は丸ごと置き換える。並びに意味があるので混ぜない。
    #[test]
    fn arrays_are_replaced_not_blended() {
        let dir = spread(&[
            ("base.toml", BASE),
            (
                "here.toml",
                r#"
extends = "base.toml"

[[ns.default.routing]]
models = ["*"]
routes = ["oauth"]
"#,
            ),
        ]);

        let routing = resolve(&at(&dir, "here.toml")).unwrap()["ns"]["default"]["routing"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(routing.len(), 1, "土台の規則は残らない: {routing:?}");
        assert_eq!(routing[0]["routes"][0].as_str(), Some("oauth"));
    }

    /// 相対パスは、それを書いたファイルの隣から解く。
    ///
    /// 起動した場所を見ていたら、この配置では土台が見つからない (試験は
    /// リポジトリの根で走る)。
    #[test]
    fn a_relative_path_is_read_from_the_file_that_wrote_it() {
        let dir = spread(&[
            ("shared/base.toml", BASE),
            (
                "faces/here.toml",
                r#"
extends = "../shared/base.toml"

[server]
listen = "127.0.0.1:8402"
"#,
            ),
        ]);

        let merged = resolve(&at(&dir, "faces/here.toml")).unwrap();
        assert_eq!(merged["server"]["listen"].as_str(), Some("127.0.0.1:8402"));
        assert_eq!(
            merged["credentials"]["bedrock"]["type"].as_str(),
            Some("bedrock_api_key")
        );
    }

    /// 何段でも重ねられる。近い方が勝つ。
    #[test]
    fn layers_stack_in_order() {
        let dir = spread(&[
            ("a.toml", BASE),
            (
                "b.toml",
                r#"
extends = "a.toml"

[server]
listen = "127.0.0.1:8402"

[routes.bedrock]
exclude = ["b が書いた"]
"#,
            ),
            (
                "c.toml",
                r#"
extends = "b.toml"

[routes.bedrock]
exclude = ["c が書いた"]
"#,
            ),
        ]);

        let merged = resolve(&at(&dir, "c.toml")).unwrap();
        assert_eq!(
            merged["routes"]["bedrock"]["exclude"].as_array().unwrap(),
            &[Value::from("c が書いた")],
            "一番手前が勝つ"
        );
        assert_eq!(
            merged["server"]["listen"].as_str(),
            Some("127.0.0.1:8402"),
            "途中の段が書いた分も効く"
        );
        assert_eq!(
            merged["credentials"]["bedrock"]["type"].as_str(),
            Some("bedrock_api_key"),
            "一番奥の段も残る"
        );
    }

    /// 自分自身を土台にしたら、読み込みが戻ってこない前に止める。
    #[test]
    fn a_file_cannot_stand_on_itself() {
        let dir = spread(&[("here.toml", "extends = \"here.toml\"\n")]);
        let e = resolve(&at(&dir, "here.toml")).unwrap_err().to_string();
        assert!(e.contains("circular"), "{e}");
        assert!(e.contains("here.toml"), "どのファイルかが分かる: {e}");
    }

    /// 遠回りの循環も止める。
    #[test]
    fn a_longer_cycle_is_caught_too() {
        let dir = spread(&[
            ("a.toml", "extends = \"b.toml\"\n"),
            ("b.toml", "extends = \"c.toml\"\n"),
            ("c.toml", "extends = \"a.toml\"\n"),
        ]);
        let e = resolve(&at(&dir, "a.toml")).unwrap_err().to_string();
        assert!(e.contains("circular"), "{e}");
    }

    /// 指した先が無ければ、どのファイルが何を指したかまで言う。
    #[test]
    fn a_missing_base_says_who_pointed_where() {
        let dir = spread(&[("here.toml", "extends = \"nowhere.toml\"\n")]);
        let e = resolve(&at(&dir, "here.toml")).unwrap_err().to_string();
        assert!(e.contains("here.toml"), "指した側: {e}");
        assert!(e.contains("nowhere.toml"), "指された先: {e}");
    }

    /// パスでない extends は、その場で言う。
    #[test]
    fn a_non_path_extends_is_rejected() {
        let dir = spread(&[("here.toml", "extends = 3\n")]);
        let e = resolve(&at(&dir, "here.toml")).unwrap_err().to_string();
        assert!(e.contains("extends"), "{e}");
        assert!(e.contains("path"), "{e}");
    }

    /// 起点そのものが無い場合も読める文言にする。
    #[test]
    fn a_missing_entry_point_is_reported_plainly() {
        let dir = TempDir::new().unwrap();
        let e = resolve(&at(&dir, "none.toml")).unwrap_err().to_string();
        assert!(e.contains("none.toml"), "{e}");
    }

    /// extends を書かないファイルは、そのまま読める。
    #[test]
    fn a_file_without_extends_is_left_alone() {
        let dir = spread(&[("alone.toml", BASE)]);
        let merged = resolve(&at(&dir, "alone.toml")).unwrap();
        assert_eq!(merged["server"]["listen"].as_str(), Some("127.0.0.1:8401"));
    }
}
