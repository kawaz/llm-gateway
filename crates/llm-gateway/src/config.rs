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

    /// クライアントが指定するモデル名ごとの経路。
    #[serde(default)]
    pub models: BTreeMap<String, ModelRoute>,
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
    },

    /// ChatGPT のサブスク OAuth。Responses API を話す。
    CodexOauth {
        #[serde(default = "chatgpt_url")]
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },

    /// 別の gateway へそのまま渡す。転送先が認証を持つので鍵は要らない。
    Relay {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
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
}

/// 1 モデルぶんの経路。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoute {
    /// upstream に送るモデル名。省略時はクライアントが指定した名前のまま。
    ///
    /// Bedrock は `anthropic.claude-fable-5` のように自分の名前空間の名前を
    /// 要求し、クライアントが送る `claude-fable-5` では 404 になる。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_name: Option<String>,

    /// 使う認証情報を優先順に並べる。上から試し、経路が断たれていれば次へ。
    pub credentials: Vec<String>,
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
    /// 起動時に落としておかないと、そのモデルを最初に使った人が
    /// 500 を踏むまで誰も気づかない。
    pub fn validate(&self) -> Result<()> {
        for (model, route) in &self.models {
            if route.credentials.is_empty() {
                return Err(Error::Config(format!(
                    "model `{model}` に credentials が指定されていません"
                )));
            }
            for name in &route.credentials {
                if !self.credentials.contains_key(name) {
                    return Err(Error::Config(format!(
                        "model `{model}` が参照する credential `{name}` が定義されていません"
                    )));
                }
            }
        }
        Ok(())
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
listen = "127.0.0.1:8319"

[store]
type = "file"

[credentials.claude-personal]
type = "claude_oauth"

[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"

[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:8317"

[models."claude-fable-5"]
upstream_name = "anthropic.claude-fable-5"
credentials = ["bedrock", "claude-personal"]

[models."claude-opus-5"]
credentials = ["claude-personal"]

[models."gpt-5.6-sol"]
credentials = ["cpa"]
"#;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s).map_err(|e| Error::Config(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn reads_a_full_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8319");
        assert_eq!(c.credentials.len(), 3);
        assert_eq!(c.models.len(), 3);
    }

    /// fable-5 は Bedrock を先に試し、駄目なら OAuth に回る。
    #[test]
    fn model_route_keeps_priority_order() {
        let c = parse(SAMPLE).unwrap();
        let route = &c.models["claude-fable-5"];
        assert_eq!(route.credentials, vec!["bedrock", "claude-personal"]);
        assert_eq!(
            route.upstream_name.as_deref(),
            Some("anthropic.claude-fable-5")
        );
    }

    /// upstream_name を書かなければクライアントの指定名をそのまま送る。
    #[test]
    fn upstream_name_is_optional() {
        let c = parse(SAMPLE).unwrap();
        assert!(c.models["claude-opus-5"].upstream_name.is_none());
    }

    #[test]
    fn credential_types_carry_their_own_fields() {
        let c = parse(SAMPLE).unwrap();

        let oauth = &c.credentials["claude-personal"];
        assert!(matches!(oauth, CredentialSpec::ClaudeOauth { .. }));
        assert_eq!(oauth.url(), "https://api.anthropic.com", "既定の宛先が入る");
        assert!(oauth.needs_secret());

        let relay = &c.credentials["cpa"];
        assert!(!relay.needs_secret(), "転送先が認証を持つので鍵は要らない");
    }

    /// 定義していない credential を参照したら起動時に落とす。
    #[test]
    fn unknown_credential_reference_is_rejected() {
        let err = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[models."m"]
credentials = ["a", "typo-here"]
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("typo-here"), "{msg}");
        assert!(msg.contains("m"), "どのモデルの話か分かる: {msg}");
    }

    #[test]
    fn empty_credential_list_is_rejected() {
        let err = parse(
            r#"
[models."m"]
credentials = []
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("m"));
    }

    /// 綴り間違いを黙って無視しない。
    #[test]
    fn unknown_key_is_rejected() {
        let err = parse(
            r#"
[server]
listen = "127.0.0.1:8319"
lisen = "typo"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("lisen"), "{err}");
    }

    #[test]
    fn unknown_credential_type_is_rejected() {
        let err = parse(
            r#"
[credentials.a]
type = "no_such_type"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no_such_type"), "{err}");
    }

    /// 空の設定でも読める (何も繋がないだけ)。
    #[test]
    fn empty_config_is_valid() {
        let c = parse("").unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8319", "既定の待ち受け先");
        assert!(c.models.is_empty());
        assert!(matches!(c.store, Store::File { dir: None }));
    }

    /// 置き場を指定すればそこを使う。
    #[test]
    fn store_dir_can_be_overridden() {
        let c = parse(
            r#"
[store]
type = "file"
dir = "/tmp/creds"
"#,
        )
        .unwrap();
        assert_eq!(c.store.resolve_dir(), PathBuf::from("/tmp/creds"));
    }

    /// 既定は state 配下。cache ではない (消えると再ログインが要るため)。
    #[test]
    fn default_store_dir_is_under_state() {
        let dir = Store::default().resolve_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with("llm-gateway/credentials"), "{s}");
        assert!(s.contains("state"), "cache ではなく state に置く: {s}");
    }

    /// Bedrock の beta 除去リストは設定で差し替えられる。
    #[test]
    fn deny_beta_can_be_overridden() {
        let c = parse(
            r#"
[credentials.b]
type = "claude_bedrock"
url = "https://example.invalid/anthropic"
deny_beta = ["some-flag"]
"#,
        )
        .unwrap();
        let CredentialSpec::ClaudeBedrock { deny_beta, .. } = &c.credentials["b"] else {
            panic!("bedrock のはず");
        };
        assert_eq!(
            deny_beta.as_deref(),
            Some(["some-flag".to_owned()].as_slice())
        );
    }

    /// 書き出して読み直すと同じになる。
    #[test]
    fn round_trips_through_toml() {
        let original = parse(SAMPLE).unwrap();
        let text = toml::to_string(&original).unwrap();
        let again = parse(&text).unwrap();

        assert_eq!(again.server.listen, original.server.listen);
        assert_eq!(again.models.len(), original.models.len());
        assert_eq!(
            again.models["claude-fable-5"].credentials,
            original.models["claude-fable-5"].credentials
        );
    }
}
