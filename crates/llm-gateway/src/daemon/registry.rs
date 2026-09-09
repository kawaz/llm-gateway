//! 走らせる 1 台 (unit) の登録簿 (DR-0028 決定 2)。
//!
//! 1 unit 1 ファイル。`$XDG_STATE_HOME/llm-gateway/daemon/units/<name>.toml` に
//! 設定ファイルの場所・実行ファイルの場所・`enabled` (desired state) を書く。
//!
//! 登録簿は**起動しない**。ここにあるのは「何を 1 台として数えるか」だけで、
//! 実際に走らせるのは `daemon run`、上げ下げを預かるのは監督者である。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 登録簿に書かれた 1 台。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    /// この台が読む設定ファイル。
    pub config: PathBuf,

    /// この台を走らせる実行ファイル。
    ///
    /// 台ごとに持つのは、同時に走る面が別のビルドでありうるため
    /// (DR-0028 決定 2)。監督者の実行ファイルと子のそれは一致しない。
    pub binary_path: PathBuf,

    /// 監督者に居てほしいか (desired state)。
    ///
    /// 「今動いているか」ではない。動いているかを知っているのは監督者だけで、
    /// 登録簿は望みの側だけを持つ。
    pub enabled: bool,

    /// 登録した時刻 (RFC 3339)。
    pub added_at: String,
}

/// 登録簿を読み書きするときに起きること。
///
/// `kind` を持つのは、CLI がそのまま JSON のエラー種別に使うため (DR-0028 決定 8)。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "`{0}` cannot be used as a unit name. the name becomes the file name <name>.toml as it is"
    )]
    BadName(String),

    #[error("there is no unit called `{name}`")]
    UnknownUnit { name: String, units: Vec<String> },

    #[error("`{name}` is already registered ({})", .config.display())]
    AlreadyRegistered { name: String, config: PathBuf },

    #[error("could not use the unit registry at {}: {reason}", .path.display())]
    Broken { path: PathBuf, reason: String },
}

impl Error {
    /// JSON に載せるエラー種別。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BadName(_) => "bad_unit_name",
            Self::UnknownUnit { .. } => "unknown_unit",
            Self::AlreadyRegistered { .. } => "unit_exists",
            Self::Broken { .. } => "registry_unreadable",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// 登録簿そのもの。
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// 置き場を指して開く。ディレクトリは書くときに作る。
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 既定の置き場で開く。
    pub fn open() -> Self {
        Self::at(default_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 1 台を足す。同じ名前が既にあれば断る。
    ///
    /// 黙って上書きすると、名前を打ち間違えた登録が既存の台を差し替えてしまう。
    pub fn add(&self, name: &str, unit: &Unit) -> Result<()> {
        check_name(name)?;
        let path = self.path_of(name);
        if let Some(existing) = self.read(&path)? {
            return Err(Error::AlreadyRegistered {
                name: name.to_owned(),
                config: existing.config,
            });
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| self.broken(&self.dir, &e))?;
        let text = toml::to_string_pretty(unit).map_err(|e| self.broken(&path, &e))?;
        std::fs::write(&path, text).map_err(|e| self.broken(&path, &e))
    }

    /// 1 台を外す。
    pub fn remove(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let path = self.path_of(name);
        if self.read(&path)?.is_none() {
            return Err(self.unknown(name));
        }
        std::fs::remove_file(&path).map_err(|e| self.broken(&path, &e))
    }

    /// 居てほしいかどうかだけを書き換える (desired state)。
    ///
    /// 書き換えるのは監督者で、CLI の `start` / `stop` はその要求にすぎない。
    /// ここに残るから、監督者を上げ直しても前回の望みから始められる。
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<Unit> {
        let mut unit = self.get(name)?;
        if unit.enabled == enabled {
            return Ok(unit);
        }
        unit.enabled = enabled;
        let path = self.path_of(name);
        let text = toml::to_string_pretty(&unit).map_err(|e| self.broken(&path, &e))?;
        std::fs::write(&path, text).map_err(|e| self.broken(&path, &e))?;
        Ok(unit)
    }

    /// 名前で 1 台を引く。
    pub fn get(&self, name: &str) -> Result<Unit> {
        check_name(name)?;
        match self.read(&self.path_of(name))? {
            Some(unit) => Ok(unit),
            None => Err(self.unknown(name)),
        }
    }

    /// 全部を名前順で。置き場が無ければ空。
    ///
    /// 名前順にするのは、`list` の並びと `--all` の並びを同じにするため。
    pub fn list(&self) -> Result<Vec<(String, Unit)>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.broken(&self.dir, &e)),
        };

        let mut units = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| self.broken(&self.dir, &e))?.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(unit) = self.read(&path)? {
                units.push((name.to_owned(), unit));
            }
        }
        units.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(units)
    }

    /// 登録されている名前だけ。エラーに添えて「では何があるのか」を言う。
    pub fn names(&self) -> Vec<String> {
        self.list()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.toml"))
    }

    fn read(&self, path: &Path) -> Result<Option<Unit>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.broken(path, &e)),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| self.broken(path, &e))
    }

    fn broken(&self, path: &Path, reason: &dyn std::fmt::Display) -> Error {
        Error::Broken {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        }
    }

    fn unknown(&self, name: &str) -> Error {
        Error::UnknownUnit {
            name: name.to_owned(),
            units: self.names(),
        }
    }
}

/// 既定の置き場。
pub fn default_dir() -> PathBuf {
    crate::config::default_state_dir()
        .join("daemon")
        .join("units")
}

/// 設定ファイルの名前から付ける既定の unit 名。
///
/// `config-11301-unstable.toml` → `config-11301-unstable`。
pub fn name_from_config(config: &Path) -> Result<String> {
    let name = config
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();
    check_name(&name)?;
    Ok(name)
}

/// 名前はそのままファイル名になる。置き場の外に書ける形を通さない。
///
/// `-` 始まりも弾く。綴りを間違えたオプションが名前として通ると、意図しない
/// 台を登録・削除してしまう。
fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains(std::path::MAIN_SEPARATOR)
        || name.starts_with('-')
    {
        return Err(Error::BadName(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(config: &str) -> Unit {
        Unit {
            config: PathBuf::from(config),
            binary_path: PathBuf::from("/usr/local/bin/llm-gateway"),
            enabled: true,
            added_at: "2026-09-09T00:00:00Z".to_owned(),
        }
    }

    fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::at(dir.path().join("units"));
        (dir, registry)
    }

    /// 書いたものが、そのまま読み戻る。
    #[test]
    fn a_registered_unit_comes_back() {
        let (_dir, registry) = registry();
        registry.add("unstable", &unit("/tmp/a.toml")).unwrap();

        assert_eq!(registry.get("unstable").unwrap(), unit("/tmp/a.toml"));
        assert_eq!(
            registry.list().unwrap(),
            vec![("unstable".to_owned(), unit("/tmp/a.toml"))]
        );
    }

    /// 置き場がまだ無い状態でも、一覧は空として答える。
    ///
    /// 「まだ 1 台も登録していない」は異常ではない。
    #[test]
    fn an_empty_registry_lists_nothing() {
        let (_dir, registry) = registry();
        assert!(registry.list().unwrap().is_empty());
        assert!(registry.names().is_empty());
    }

    /// 一覧は名前順。`--all` の順を毎回同じにするため。
    #[test]
    fn units_are_listed_by_name() {
        let (_dir, registry) = registry();
        registry.add("stable", &unit("/tmp/s.toml")).unwrap();
        registry.add("unstable", &unit("/tmp/u.toml")).unwrap();

        assert_eq!(registry.names(), vec!["stable", "unstable"]);
    }

    /// 同じ名前は黙って上書きしない。打ち間違えが既存の台を差し替える。
    #[test]
    fn a_duplicate_name_is_refused() {
        let (_dir, registry) = registry();
        registry.add("a", &unit("/tmp/a.toml")).unwrap();

        let e = registry.add("a", &unit("/tmp/b.toml")).unwrap_err();
        assert_eq!(e.kind(), "unit_exists");
        assert!(e.to_string().contains("/tmp/a.toml"), "{e}");
        // 既存の側は動いていない。
        assert_eq!(
            registry.get("a").unwrap().config,
            PathBuf::from("/tmp/a.toml")
        );
    }

    /// 知らない名前には、では何があるのかを添えて断る。
    #[test]
    fn an_unknown_unit_says_what_is_registered() {
        let (_dir, registry) = registry();
        registry.add("stable", &unit("/tmp/s.toml")).unwrap();

        let e = registry.get("nope").unwrap_err();
        assert_eq!(e.kind(), "unknown_unit");
        let Error::UnknownUnit { units, .. } = &e else {
            panic!("{e}");
        };
        assert_eq!(units, &["stable".to_owned()]);

        assert_eq!(registry.remove("nope").unwrap_err().kind(), "unknown_unit");
    }

    #[test]
    fn a_removed_unit_is_gone() {
        let (_dir, registry) = registry();
        registry.add("a", &unit("/tmp/a.toml")).unwrap();
        registry.remove("a").unwrap();

        assert!(registry.list().unwrap().is_empty());
    }

    /// 望みだけを書き換える。設定も実行ファイルも、登録した時のまま残る。
    #[test]
    fn only_the_desired_state_is_rewritten() {
        let (_dir, registry) = registry();
        registry.add("a", &unit("/tmp/a.toml")).unwrap();

        assert!(!registry.set_enabled("a", false).unwrap().enabled);
        let stored = registry.get("a").unwrap();
        assert!(!stored.enabled);
        assert_eq!(stored.config, PathBuf::from("/tmp/a.toml"));
        assert_eq!(stored.added_at, unit("/tmp/a.toml").added_at);

        // 同じ値を書き直しても壊れない (`start` を 2 回受けても同じ)。
        assert!(!registry.set_enabled("a", false).unwrap().enabled);
        assert!(registry.set_enabled("a", true).unwrap().enabled);

        assert_eq!(
            registry.set_enabled("nope", true).unwrap_err().kind(),
            "unknown_unit"
        );
    }

    /// 名前はファイル名になる。置き場の外を指す形を通さない。
    #[test]
    fn names_that_escape_the_directory_are_refused() {
        let (_dir, registry) = registry();
        for bad in ["", ".", "..", "../../etc/passwd", "sub/name", "-name"] {
            assert_eq!(
                registry.add(bad, &unit("/tmp/a.toml")).unwrap_err().kind(),
                "bad_unit_name",
                "{bad}"
            );
            assert_eq!(
                registry.get(bad).unwrap_err().kind(),
                "bad_unit_name",
                "{bad}"
            );
        }
    }

    /// 壊れたファイルは「無い」ことにしない。読めないと言う。
    #[test]
    fn a_broken_file_is_reported_not_skipped() {
        let (_dir, registry) = registry();
        std::fs::create_dir_all(registry.dir()).unwrap();
        std::fs::write(registry.dir().join("a.toml"), "this is not toml =").unwrap();

        assert_eq!(registry.get("a").unwrap_err().kind(), "registry_unreadable");
        assert_eq!(registry.list().unwrap_err().kind(), "registry_unreadable");
    }

    /// toml 以外は登録簿の中身ではない (エディタの残骸などを拾わない)。
    #[test]
    fn other_files_are_not_units() {
        let (_dir, registry) = registry();
        registry.add("a", &unit("/tmp/a.toml")).unwrap();
        std::fs::write(registry.dir().join("a.toml.bak"), "junk").unwrap();

        assert_eq!(registry.names(), vec!["a"]);
    }

    /// 既定の名前は、設定ファイル名から拡張子を落としたもの。
    #[test]
    fn the_default_name_comes_from_the_configuration_file() {
        assert_eq!(
            name_from_config(Path::new("/x/config-11301-unstable.toml")).unwrap(),
            "config-11301-unstable"
        );
        assert_eq!(name_from_config(Path::new("plain")).unwrap(), "plain");
        // 名前の取りようが無い指定は、空の名前で登録せずに断る。
        assert_eq!(
            name_from_config(Path::new("/")).unwrap_err().kind(),
            "bad_unit_name"
        );
    }

    /// 既定の置き場は state の下 (消えると再登録が要るので cache ではない)。
    #[test]
    fn the_default_directory_sits_under_the_state_directory() {
        let dir = default_dir();
        assert!(
            dir.ends_with("llm-gateway/daemon/units"),
            "{}",
            dir.display()
        );
    }
}
