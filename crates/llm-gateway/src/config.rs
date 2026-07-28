//! 設定ファイル。
//!
//! 既定の置き場は `$XDG_CONFIG_HOME/llm-gateway/config.toml`。`--config` で
//! 別の場所を指せる。
//!
//! ```toml
//! [server]
//! listen = "127.0.0.1:8319"
//!
//! [store]
//! type = "file"
//! # dir 省略時は $XDG_STATE_HOME/llm-gateway/credentials
//!
//! # 認証情報の中身 (token 等) はここに書かない。store に置いた
//! # <key>.json を type と結びつけるだけ。
//! [credentials.claude-personal]
//! type = "claude_oauth"
//!
//! [credentials.bedrock]
//! type = "claude_bedrock"
//! url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
//!
//! [credentials.cpa]
//! type = "relay"
//! url = "http://127.0.0.1:8317"
//!
//! # モデルごとに、使う認証情報を優先順に並べる。上から試す。
//! [models."claude-fable-5"]
//! upstream_name = "anthropic.claude-fable-5"
//! credentials = ["bedrock", "claude-personal"]
//!
//! [models."gpt-5.6-sol"]
//! credentials = ["cpa"]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: Server,

    #[serde(default)]
    pub store: Store,

    /// 認証情報の宣言。キーが store 内のファイル名 (`<key>.json`) になる。
    #[serde(default)]
    pub credentials: BTreeMap<String, CredentialSpec>,

    /// 公開するモデルの絞り込み。全 credential に効く。
    ///
    /// upstream から取れる一覧をそのまま出すと古い世代まで並ぶので、
    /// ここで隠す。`claude-opus-4*` のように書ける。
    ///
    /// TOML の性質上、テーブルより前に書く必要がある。読む側が混乱しないよう
    /// `[filter]` テーブルに入れてある。
    #[serde(default)]
    pub filter: Filter,

    /// モデルごとの経路。パターンで書ける。
    ///
    /// 上から順に照合し、最初に当たったものを使う。書かれていないモデルは
    /// `credentials` の宣言順に試す。
    ///
    /// ```toml
    /// [[routing]]
    /// models = ["claude-fable-*"]
    /// credentials = ["bedrock", "claude-work-b", "claude-personal"]
    /// ```
    #[serde(default)]
    pub routing: Vec<RoutingRule>,

    /// 短い名前。値はパターンで、当たるもののうち一番新しいものに向く。
    ///
    /// 既定で `fable` / `opus` / `sonnet` / `haiku` が入る。ここに書けば
    /// 上書きでき、新しい名前も足せる。
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,

    #[serde(default)]
    pub discovery: Discovery,
}

/// 公開するモデルの絞り込み。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// 隠すモデル。`claude-opus-4*` のようにパターンで書ける。
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// 1 つの振り分け規則。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRule {
    /// 対象のモデル。パターンで書ける。
    pub models: Vec<String>,
    /// 使う認証情報を優先順に。上から試す。
    pub credentials: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    /// 一覧を取り直す間隔 (秒)。
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            refresh_secs: default_refresh_secs(),
        }
    }
}

fn default_refresh_secs() -> u64 {
    3600
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// 待ち受け先。cpa (8317/8318) と衝突しない番号にする。
    #[serde(default = "default_listen")]
    pub listen: String,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8319".to_owned()
}

/// 認証情報の置き場。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Store {
    /// 1 認証情報 1 ファイル。平文。
    File {
        /// 省略時は `$XDG_STATE_HOME/llm-gateway/credentials`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<PathBuf>,
    },
}

impl Default for Store {
    fn default() -> Self {
        Self::File { dir: None }
    }
}

impl Store {
    /// 実際に使う置き場。
    pub fn resolve_dir(&self) -> PathBuf {
        match self {
            Self::File { dir: Some(d) } => d.clone(),
            Self::File { dir: None } => default_credentials_dir(),
        }
    }
}

/// 認証情報 1 件の宣言。
///
/// 秘密そのものはここに書かない。store 側の `<key>.json` が持つ。
/// ここにあるのは「どう認証して、どこへ繋ぐか」だけ。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialSpec {
    /// Anthropic のサブスク OAuth。`Authorization: Bearer`。
    ClaudeOauth {
        #[serde(default = "anthropic_url")]
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        /// この認証情報で扱わないモデル。全体の `exclude` に足して効く。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },

    /// Bedrock の Anthropic 互換。`x-api-key`。
    ///
    /// upstream が受け付けない beta フラグを落とす。落とす顔ぶれは
    /// クライアントの更新で変わるので、既定値を上書きできるようにしてある。
    ClaudeBedrock {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        /// 省略時は実測済みの既定リスト。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deny_beta: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },

    /// ChatGPT のサブスク OAuth。Responses API を話す。
    CodexOauth {
        #[serde(default = "chatgpt_url")]
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },

    /// 別の gateway へそのまま渡す。転送先が認証を持つので鍵は要らない。
    ///
    /// 転送先の一覧は聞きに行けないので、扱うモデルをここに書く。
    Relay {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        /// 転送するモデル。パターンで書ける。
        #[serde(default)]
        models: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },
}

fn anthropic_url() -> String {
    "https://api.anthropic.com".to_owned()
}

fn chatgpt_url() -> String {
    "https://chatgpt.com/backend-api/codex".to_owned()
}

impl CredentialSpec {
    /// store から秘密を読む必要があるか。
    pub fn needs_secret(&self) -> bool {
        !matches!(self, Self::Relay { .. })
    }

    pub fn url(&self) -> &str {
        match self {
            Self::ClaudeOauth { url, .. }
            | Self::ClaudeBedrock { url, .. }
            | Self::CodexOauth { url, .. }
            | Self::Relay { url, .. } => url,
        }
    }

    pub fn headers(&self) -> &BTreeMap<String, String> {
        match self {
            Self::ClaudeOauth { headers, .. }
            | Self::ClaudeBedrock { headers, .. }
            | Self::CodexOauth { headers, .. }
            | Self::Relay { headers, .. } => headers,
        }
    }

    pub fn exclude(&self) -> &[String] {
        match self {
            Self::ClaudeOauth { exclude, .. }
            | Self::ClaudeBedrock { exclude, .. }
            | Self::CodexOauth { exclude, .. }
            | Self::Relay { exclude, .. } => exclude,
        }
    }

    /// upstream に一覧を聞けるか。聞けないものは設定に書いてもらう。
    pub fn discovery_flavor(&self) -> Option<crate::discovery::Flavor> {
        match self {
            Self::ClaudeOauth { .. } => Some(crate::discovery::Flavor::Anthropic),
            Self::ClaudeBedrock { .. } => Some(crate::discovery::Flavor::Bedrock),
            // ChatGPT の Responses API に一覧の口は無い。relay 先も聞けない。
            Self::CodexOauth { .. } | Self::Relay { .. } => None,
        }
    }

    /// 一覧を聞けない upstream で、扱うモデルとして宣言されたもの。
    pub fn declared_models(&self) -> &[String] {
        match self {
            Self::Relay { models, .. } => models,
            _ => &[],
        }
    }
}

impl Config {
    /// 読み込む。設定に矛盾があればここで弾く。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("{} を読めません: {e}", path.display())))?;
        let config: Self = toml::from_str(&raw)
            .map_err(|e| Error::Config(format!("{} の内容が不正です: {e}", path.display())))?;
        config.validate()?;
        Ok(config)
    }

    /// 参照先が揃っているかを確かめる。
    ///
    /// 起動時に落としておかないと、その規則を最初に踏んだ人が
    /// 500 を見るまで誰も気づかない。
    pub fn validate(&self) -> Result<()> {
        for (i, rule) in self.routing.iter().enumerate() {
            if rule.models.is_empty() {
                return Err(Error::Config(format!(
                    "routing[{i}] に models が指定されていません"
                )));
            }
            if rule.credentials.is_empty() {
                return Err(Error::Config(format!(
                    "routing[{i}] ({}) に credentials が指定されていません",
                    rule.models.join(", ")
                )));
            }
            for name in &rule.credentials {
                if !self.credentials.contains_key(name) {
                    return Err(Error::Config(format!(
                        "routing[{i}] ({}) が参照する credential `{name}` が定義されていません",
                        rule.models.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }

    /// 実際に使うエイリアス。既定に設定を重ねる。
    pub fn resolved_aliases(&self) -> BTreeMap<String, String> {
        let mut all = crate::discovery::default_aliases();
        all.extend(self.aliases.clone());
        all
    }

    /// このモデルを扱う認証情報を、試す順に返す。
    ///
    /// `routing` の上から照合し、最初に当たった規則を使う。当たらなければ
    /// 宣言順に全部試す。
    pub fn credentials_for(&self, model: &str) -> Vec<&str> {
        for rule in &self.routing {
            if crate::pattern::matches_any(&rule.models, model) {
                return rule.credentials.iter().map(String::as_str).collect();
            }
        }
        self.credentials.keys().map(String::as_str).collect()
    }

    /// この認証情報がこのモデルを扱ってよいか。
    pub fn allows(&self, credential: &str, model: &str) -> bool {
        if crate::pattern::matches_any(&self.filter.exclude, model) {
            return false;
        }
        match self.credentials.get(credential) {
            Some(spec) => !crate::pattern::matches_any(spec.exclude(), model),
            None => false,
        }
    }

    /// 既定の設定ファイルの場所。
    pub fn default_path() -> PathBuf {
        xdg_dir("XDG_CONFIG_HOME", ".config")
            .join("llm-gateway")
            .join("config.toml")
    }
}

/// 既定の認証情報の置き場。
///
/// cache でなく state に置く。refresh token は消えると再ログインが要るので、
/// 「消えても再生成できる」cache の扱いに合わない。
pub fn default_credentials_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("llm-gateway")
        .join("credentials")
}

fn xdg_dir(env: &str, fallback: &str) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 実運用を想定した一式。
    const SAMPLE: &str = r#"
[server]
listen = "127.0.0.1:8317"

[store]
type = "file"

# 古い世代は出さない。
[filter]
exclude = ["claude-3-*", "claude-opus-4*", "claude-sonnet-4-*"]

[credentials.claude-personal]
type = "claude_oauth"

[credentials.claude-work-a]
type = "claude_oauth"

# fable 専用のアカウント。
[credentials.claude-work-b]
type = "claude_oauth"
exclude = ["claude-opus-*", "claude-sonnet-*", "claude-haiku-*"]

[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"

[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:8320"
models = ["gpt-*"]

# fable は Bedrock 優先 (Claude アカウントを消費しない)。
[[routing]]
models = ["claude-fable-*"]
credentials = ["bedrock", "claude-work-b", "claude-personal"]

[[routing]]
models = ["gpt-*"]
credentials = ["cpa"]

[aliases]
o = "claude-opus-*"
"#;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s).map_err(|e| Error::Config(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn reads_a_full_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8317");
        assert_eq!(c.credentials.len(), 5);
        assert_eq!(c.routing.len(), 2);
        assert_eq!(c.discovery.refresh_secs, 3600, "既定は 1 時間");
    }

    /// モデル一覧を手で書かない。upstream に聞くので、設定にあるのは
    /// 「隠すもの」と「特別扱いするもの」だけ。
    #[test]
    fn no_model_list_is_required() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"
"#,
        )
        .unwrap();
        assert!(c.routing.is_empty());
        assert!(c.filter.exclude.is_empty());
    }

    /// 全体の exclude は全 credential に効く。
    #[test]
    fn global_exclude_hides_old_generations() {
        let c = parse(SAMPLE).unwrap();
        for hidden in [
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-3-5-sonnet-20241022",
        ] {
            assert!(!c.allows("claude-personal", hidden), "{hidden} は隠す");
        }
        for shown in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5"] {
            assert!(c.allows("claude-personal", shown), "{shown} は出す");
        }
    }

    /// credential ごとの exclude はその credential にだけ効く。
    #[test]
    fn per_credential_exclude_is_scoped() {
        let c = parse(SAMPLE).unwrap();
        assert!(
            !c.allows("claude-work-b", "claude-opus-5"),
            "fable 専用なので opus は扱わない"
        );
        assert!(c.allows("claude-work-b", "claude-fable-5"), "fable は扱う");
        assert!(
            c.allows("claude-personal", "claude-opus-5"),
            "他の credential には影響しない"
        );
    }

    #[test]
    fn unknown_credential_is_not_allowed() {
        let c = parse(SAMPLE).unwrap();
        assert!(!c.allows("no-such-credential", "claude-opus-5"));
    }

    /// ルーティングはパターンで書ける。最初に当たった規則を使う。
    #[test]
    fn routing_matches_by_pattern() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            c.credentials_for("claude-fable-5"),
            vec!["bedrock", "claude-work-b", "claude-personal"]
        );
        assert_eq!(c.credentials_for("gpt-5.6-sol"), vec!["cpa"]);
    }

    /// 規則に当たらないモデルは宣言順に試す。
    /// 新しいモデルが出ても、設定を触らずに使える。
    #[test]
    fn unmatched_models_fall_back_to_declaration_order() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            c.credentials_for("claude-opus-6"),
            vec![
                "bedrock",
                "claude-personal",
                "claude-work-a",
                "claude-work-b",
                "cpa"
            ],
            "宣言順 (BTreeMap なので名前順)"
        );
    }

    /// 上に書いた規則が勝つ。
    #[test]
    fn first_matching_rule_wins() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[credentials.b]
type = "claude_oauth"

[[routing]]
models = ["claude-opus-5"]
credentials = ["a"]

[[routing]]
models = ["claude-*"]
credentials = ["b"]
"#,
        )
        .unwrap();
        assert_eq!(
            c.credentials_for("claude-opus-5"),
            vec!["a"],
            "個別指定が先"
        );
        assert_eq!(
            c.credentials_for("claude-sonnet-5"),
            vec!["b"],
            "後ろの広い規則"
        );
    }

    /// エイリアスは設定に書いたものだけ。コードに既定は無い。
    #[test]
    fn aliases_come_only_from_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.resolved_aliases()["o"], "claude-opus-*");

        let empty = parse("").unwrap();
        assert!(
            empty.resolved_aliases().is_empty(),
            "書かなければ 1 つも無い"
        );
    }

    #[test]
    fn aliases_are_read_as_written() {
        let c = parse(
            r#"
[aliases]
opus = "claude-opus-4*"
"#,
        )
        .unwrap();
        assert_eq!(c.resolved_aliases()["opus"], "claude-opus-4*");
    }

    /// 一覧を聞ける upstream と聞けない upstream。
    #[test]
    fn discovery_flavor_by_type() {
        use crate::discovery::Flavor;
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            c.credentials["claude-personal"].discovery_flavor(),
            Some(Flavor::Anthropic)
        );
        assert_eq!(
            c.credentials["bedrock"].discovery_flavor(),
            Some(Flavor::Bedrock)
        );
        assert_eq!(
            c.credentials["cpa"].discovery_flavor(),
            None,
            "転送先には聞けない"
        );
    }

    /// 聞けない upstream は設定に書いたモデルを使う。
    #[test]
    fn relay_declares_its_models() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.credentials["cpa"].declared_models(), ["gpt-*"]);
        assert!(
            c.credentials["claude-personal"]
                .declared_models()
                .is_empty()
        );
    }

    #[test]
    fn unknown_credential_reference_is_rejected() {
        let err = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[[routing]]
models = ["m"]
credentials = ["a", "typo-here"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo-here"), "{err}");
    }

    #[test]
    fn empty_routing_fields_are_rejected() {
        assert!(parse("[[routing]]\nmodels = []\ncredentials = []").is_err());
        assert!(
            parse("[[routing]]\nmodels = [\"m\"]\ncredentials = []")
                .unwrap_err()
                .to_string()
                .contains('m')
        );
    }

    /// 綴り間違いを黙って無視しない。
    #[test]
    fn unknown_key_is_rejected() {
        let err = parse("[server]\nlisen = \"typo\"").unwrap_err();
        assert!(err.to_string().contains("lisen"), "{err}");
    }

    #[test]
    fn unknown_credential_type_is_rejected() {
        let err = parse("[credentials.a]\ntype = \"no_such_type\"").unwrap_err();
        assert!(err.to_string().contains("no_such_type"), "{err}");
    }

    #[test]
    fn empty_config_is_valid() {
        let c = parse("").unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8319");
        assert!(matches!(c.store, Store::File { dir: None }));
    }

    #[test]
    fn store_dir_can_be_overridden() {
        let c = parse("[store]\ntype = \"file\"\ndir = \"/tmp/creds\"").unwrap();
        assert_eq!(c.store.resolve_dir(), PathBuf::from("/tmp/creds"));
    }

    /// 既定は state 配下。cache ではない (消えると再ログインが要る)。
    #[test]
    fn default_store_dir_is_under_state() {
        let dir = Store::default().resolve_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with("llm-gateway/credentials"), "{s}");
        assert!(s.contains("state"), "{s}");
    }

    #[test]
    fn discovery_interval_can_be_set() {
        let c = parse("[discovery]\nrefresh_secs = 300").unwrap();
        assert_eq!(c.discovery.refresh_secs, 300);
    }

    #[test]
    fn round_trips_through_toml() {
        let original = parse(SAMPLE).unwrap();
        let again = parse(&toml::to_string(&original).unwrap()).unwrap();
        assert_eq!(again.routing.len(), original.routing.len());
        assert_eq!(again.filter.exclude, original.filter.exclude);
        assert_eq!(
            again.credentials_for("claude-fable-5"),
            original.credentials_for("claude-fable-5")
        );
    }
}

#[cfg(test)]
mod example_tests {
    use super::*;

    /// 配る雛形が実際に読めるか。
    ///
    /// 壊れていると `just init-config` の直後に起動できず、初めて使う人が
    /// 最初につまずく。設定の書き方を変えたときに追従を忘れやすい。
    #[test]
    fn shipped_example_is_valid() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../dist/config.example.toml"
        );
        let raw = std::fs::read_to_string(path).expect("dist/config.example.toml が要る");
        let config: Config = toml::from_str(&raw).expect("雛形が壊れている");
        config.validate().expect("雛形の参照が壊れている");

        // 使い方を示す要素が一通り入っているか。
        assert!(!config.filter.exclude.is_empty(), "隠し方の例");
        assert!(!config.routing.is_empty(), "振り分けの例");
        assert!(!config.aliases.is_empty(), "短い名前の例");
        assert!(
            config.credentials.values().any(|c| !c.exclude().is_empty()),
            "credential ごとの絞り込みの例"
        );
        assert!(
            config
                .credentials
                .values()
                .any(|c| !c.declared_models().is_empty()),
            "relay で扱うモデルを宣言する例"
        );
    }

    /// 雛形にモデル一覧を書かない。
    ///
    /// 一覧は upstream に聞くのが本筋で、雛形が手書きの例を示すと
    /// そちらに引きずられる。
    #[test]
    fn example_does_not_hardcode_a_model_list() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../dist/config.example.toml"
        );
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(!raw.contains("[models."), "旧形式のモデル定義が残っている");
    }
}
