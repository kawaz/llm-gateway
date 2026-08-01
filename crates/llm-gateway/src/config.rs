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
//! [stats]
//! # dir 省略時は $XDG_STATE_HOME/llm-gateway/stats
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
//! # 何を公開し、どこへ流すかは namespace ごとに書く。`default` は
//! # `/v1/...` (namespace を書かないパス) が解決される先で、他と同じ書式。
//! # `auth_token` を書かないと誰も通れない。
//! [ns.default]
//! auth_token = "十分に長い乱数"
//!
//! [ns.default.filter]
//! exclude = ["claude-3-*"]
//!
//! # モデルごとに、使う認証情報を優先順に並べる。上から試す。
//! [[ns.default.routing]]
//! models = ["claude-fable-*"]
//! credentials = ["bedrock", "claude-personal"]
//!
//! [[ns.default.routing]]
//! models = ["gpt-*"]
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

    #[serde(default)]
    pub stats: Stats,

    /// 認証情報の宣言。キーが store 内のファイル名 (`<key>.json`) になる。
    ///
    /// **全 namespace で共有する。** 同じアカウントを namespace ごとに
    /// 持つと、token の更新が競合するうえ何度もログインが要る。
    #[serde(default)]
    pub credentials: BTreeMap<String, CredentialSpec>,

    /// upstream への問い合わせ設定。
    ///
    /// これも共有。同じアカウントに namespace の数だけ聞きに行っても、
    /// 返ってくる一覧は同じ。
    #[serde(default)]
    pub discovery: Discovery,

    /// 名前空間。`/ns-<名前>/v1/messages` で使い分ける。
    ///
    /// 何を隠すか・どう振り分けるか・短い名前をどうするかは、使う人ごとに
    /// 違う。そこだけ分ける。
    ///
    /// `default` も特別扱いしない。namespace を書かないパス (`/v1/...`) は
    /// `default` に解決されるので、`[ns.default]` を書かなければ `/v1/...` は
    /// 404 になる (DR-0006)。
    #[serde(default, rename = "ns")]
    pub namespaces: BTreeMap<String, Namespace>,
}

/// 1 つの名前空間。使う人ごとに変えたいものだけを持つ。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Namespace {
    /// 公開するモデルの絞り込み。
    ///
    /// upstream から取れる一覧をそのまま出すと古い世代まで並ぶので、
    /// ここで隠す。`claude-opus-4*` のように書ける。
    #[serde(default)]
    pub filter: Filter,

    /// モデルごとの経路。パターンで書ける。
    ///
    /// 上から順に照合し、最初に当たったものを使う。書かれていないモデルは
    /// `credentials` の宣言順に試す。
    #[serde(default)]
    pub routing: Vec<RoutingRule>,

    /// 短い名前。値はパターンで、当たるもののうち一番新しいものに向く。
    /// 別の短い名前を指してもよい。
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,

    /// この namespace を使うのに要るトークン。
    ///
    /// クライアントの `Authorization` をこれと突き合わせる。**書かなければ
    /// 誰でも通す** (DR-0006)。手前 (tailnet / リバースプロキシ) で境界を
    /// 引く運用では、ここで二重に認証を求める意味がないうえ、クライアントに
    /// トークンを持たせること自体が邪魔になる (Claude Code は
    /// `ANTHROPIC_AUTH_TOKEN` があるとサブスクとしての振る舞いをやめる)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    /// この namespace で使う認証情報。空なら全部。
    ///
    /// 面ごとにアカウントを分けたいときに絞る。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<String>,
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

/// 使用量の日次集計の置き場 (DR-0011)。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    /// 省略時は `$XDG_STATE_HOME/llm-gateway/stats`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

impl Stats {
    /// 実際に使う置き場。
    pub fn resolve_dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(default_stats_dir)
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

    /// config.toml に書く `type` と同じ語。表示に使う。
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ClaudeOauth { .. } => "claude_oauth",
            Self::ClaudeBedrock { .. } => "claude_bedrock",
            Self::CodexOauth { .. } => "codex_oauth",
            Self::Relay { .. } => "relay",
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
        for (name, ns) in &self.namespaces {
            if name.is_empty() {
                return Err(Error::Config("namespace 名が空です".to_owned()));
            }
            self.validate_namespace(name, ns)?;
        }
        Ok(())
    }

    fn validate_namespace(&self, ns_name: &str, ns: &Namespace) -> Result<()> {
        let known = |name: &String| self.credentials.contains_key(name);

        for name in &ns.credentials {
            if !known(name) {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` が参照する credential `{name}` が定義されていません"
                )));
            }
        }
        for (i, rule) in ns.routing.iter().enumerate() {
            if rule.models.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` の routing[{i}] に models が指定されていません"
                )));
            }
            if rule.credentials.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` の routing[{i}] ({}) に credentials が指定されていません",
                    rule.models.join(", ")
                )));
            }
            for name in &rule.credentials {
                if !known(name) {
                    return Err(Error::Config(format!(
                        "namespace `{ns_name}` の routing[{i}] ({}) が参照する credential `{name}` が定義されていません",
                        rule.models.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }

    /// 名前で namespace を引く。書いていなければ無い。
    ///
    /// パスに namespace が無いリクエストは `default` を引く。`[ns.default]` を
    /// 書かなければここで None になり、呼び出し側が 404 を返す (DR-0006)。
    pub fn namespace(&self, name: &str) -> Option<&Namespace> {
        self.namespaces.get(name)
    }

    /// 公開している namespace 名。
    pub fn namespace_names(&self) -> Vec<&str> {
        self.namespaces.keys().map(String::as_str).collect()
    }

    /// 既定の設定ファイルの場所。
    pub fn default_path() -> PathBuf {
        xdg_dir("XDG_CONFIG_HOME", ".config")
            .join("llm-gateway")
            .join("config.toml")
    }
}

/// パスに namespace が無いときに使う名前。
pub const DEFAULT_NAMESPACE: &str = "default";

impl Namespace {
    /// 実際に使うエイリアス。
    pub fn resolved_aliases(&self) -> BTreeMap<String, String> {
        let mut all = crate::discovery::default_aliases();
        all.extend(self.aliases.clone());
        all
    }

    /// このモデルを扱う認証情報を、試す順に返す。
    ///
    /// `routing` の上から照合し、最初に当たった規則を使う。当たらなければ
    /// この namespace が使える認証情報を宣言順に全部試す。
    pub fn credentials_for<'a>(&'a self, model: &str, all: &'a Config) -> Vec<&'a str> {
        for rule in &self.routing {
            if crate::pattern::matches_any(&rule.models, model) {
                return rule.credentials.iter().map(String::as_str).collect();
            }
        }
        self.usable_credentials(all)
    }

    /// この namespace が使ってよい認証情報。
    pub fn usable_credentials<'a>(&'a self, all: &'a Config) -> Vec<&'a str> {
        if self.credentials.is_empty() {
            return all.credentials.keys().map(String::as_str).collect();
        }
        self.credentials.iter().map(String::as_str).collect()
    }

    /// この認証情報がこのモデルを扱ってよいか。
    pub fn allows(&self, credential: &str, model: &str, all: &Config) -> bool {
        if crate::pattern::matches_any(&self.filter.exclude, model) {
            return false;
        }
        if !self.usable_credentials(all).contains(&credential) {
            return false;
        }
        match all.credentials.get(credential) {
            Some(spec) => !crate::pattern::matches_any(spec.exclude(), model),
            None => false,
        }
    }

    /// クライアントが名乗ったトークンを検査する。
    ///
    /// `auth_token` を書いていない namespace は、トークンの有無にかかわらず
    /// 通す (DR-0006)。境界は手前で引く前提。
    pub fn authorize(&self, presented: Option<&str>) -> Authorization {
        let Some(expected) = &self.auth_token else {
            return Authorization::Open;
        };
        // `Bearer xxx` でも `xxx` でも受ける。クライアントによって送り方が違う。
        let matched = presented.is_some_and(|p| {
            let p = p.strip_prefix("Bearer ").unwrap_or(p).trim();
            p == expected
        });
        if matched {
            Authorization::Accepted
        } else {
            Authorization::WrongToken
        }
    }
}

/// トークン検査の結果。
///
/// bool で返すと「なぜ通ったか」が消える。トークンが合って通ったのと、
/// そもそも検査していないのとでは意味が違い、記録に残す価値も違う
/// (DR-0006)。列挙にしておくと `match` が網羅を強制するので、通す枝と
/// 拒む枝のどちらも書き落とせない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// トークンが合っている。
    Accepted,
    /// トークンが違う (名乗っていない場合を含む)。
    WrongToken,
    /// この namespace は誰でも通す (`auth_token` を書いていない)。
    Open,
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

/// 既定の日次集計の置き場。
///
/// 認証情報と同じく state に置く。過去日の集計は消えると**作り直せない**
/// (upstream に聞き直す口が無い) ので、cache の扱いに合わない (DR-0011)。
pub fn default_stats_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("llm-gateway")
        .join("stats")
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

# 古い世代は出さない。
[ns.default.filter]
exclude = ["claude-3-*", "claude-opus-4*", "claude-sonnet-4-*"]

# fable は Bedrock 優先 (Claude アカウントを消費しない)。
[[ns.default.routing]]
models = ["claude-fable-*"]
credentials = ["bedrock", "claude-work-b", "claude-personal"]

[[ns.default.routing]]
models = ["gpt-*"]
credentials = ["cpa"]

[ns.default.aliases]
o = "claude-opus-*"
"#;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s).map_err(|e| Error::Config(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    /// 既定の namespace。`/v1/...` が解決される先。
    pub(super) fn ns(c: &Config) -> &Namespace {
        c.namespace(DEFAULT_NAMESPACE)
            .expect("この設定には [ns.default] がある")
    }

    /// `auth_token` を書かない namespace は、名乗り方に関わらず誰でも通す。
    ///
    /// 境界は手前 (tailnet / リバースプロキシ) で引く前提 (DR-0006)。
    #[test]
    fn namespace_without_token_accepts_everyone() {
        let ns = Namespace::default();
        assert_eq!(ns.auth_token, None, "書いていない状態");

        for presented in [None, Some("anything"), Some("Bearer anything"), Some("")] {
            assert_eq!(
                ns.authorize(presented),
                Authorization::Open,
                "{presented:?} も通す"
            );
        }
    }

    /// 書いてあれば、合っているものだけ通す。
    ///
    /// `Bearer xxx` と裸の `xxx` の両方を受けるのは、クライアントによって
    /// 送り方が違うため。
    #[test]
    fn configured_token_is_matched_both_ways() {
        let ns = Namespace {
            auth_token: Some("s3cret".to_owned()),
            ..Namespace::default()
        };

        for ok in ["s3cret", "Bearer s3cret"] {
            assert_eq!(ns.authorize(Some(ok)), Authorization::Accepted, "{ok}");
        }
        for bad in ["", "nope", "Bearer nope", "bearer s3cret", "s3cretx"] {
            assert_eq!(ns.authorize(Some(bad)), Authorization::WrongToken, "{bad}");
        }
        assert_eq!(
            ns.authorize(None),
            Authorization::WrongToken,
            "名乗らないのは不一致と同じ扱い"
        );
    }

    /// 書いていない `default` は存在しない。`/v1/...` は 404 になる。
    #[test]
    fn default_namespace_is_not_conjured_up() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[ns.personal]
"#,
        )
        .unwrap();

        assert!(c.namespace(DEFAULT_NAMESPACE).is_none());
        assert_eq!(
            c.namespace_names(),
            vec!["personal"],
            "書いた分しか公開しない"
        );
    }

    #[test]
    fn reads_a_full_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8317");
        assert_eq!(c.credentials.len(), 5);
        assert_eq!(ns(&c).routing.len(), 2);
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

[ns.default]
"#,
        )
        .unwrap();
        assert!(ns(&c).routing.is_empty());
        assert!(ns(&c).filter.exclude.is_empty());
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
            assert!(
                !ns(&c).allows("claude-personal", hidden, &c),
                "{hidden} は隠す"
            );
        }
        for shown in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5"] {
            assert!(
                ns(&c).allows("claude-personal", shown, &c),
                "{shown} は出す"
            );
        }
    }

    /// credential ごとの exclude はその credential にだけ効く。
    #[test]
    fn per_credential_exclude_is_scoped() {
        let c = parse(SAMPLE).unwrap();
        assert!(
            !ns(&c).allows("claude-work-b", "claude-opus-5", &c),
            "fable 専用なので opus は扱わない"
        );
        assert!(
            ns(&c).allows("claude-work-b", "claude-fable-5", &c),
            "fable は扱う"
        );
        assert!(
            ns(&c).allows("claude-personal", "claude-opus-5", &c),
            "他の credential には影響しない"
        );
    }

    #[test]
    fn unknown_credential_is_not_allowed() {
        let c = parse(SAMPLE).unwrap();
        assert!(!ns(&c).allows("no-such-credential", "claude-opus-5", &c));
    }

    /// ルーティングはパターンで書ける。最初に当たった規則を使う。
    #[test]
    fn routing_matches_by_pattern() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).credentials_for("claude-fable-5", &c),
            vec!["bedrock", "claude-work-b", "claude-personal"]
        );
        assert_eq!(ns(&c).credentials_for("gpt-5.6-sol", &c), vec!["cpa"]);
    }

    /// 規則に当たらないモデルは宣言順に試す。
    /// 新しいモデルが出ても、設定を触らずに使える。
    #[test]
    fn unmatched_models_fall_back_to_declaration_order() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).credentials_for("claude-opus-6", &c),
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

[[ns.default.routing]]
models = ["claude-opus-5"]
credentials = ["a"]

[[ns.default.routing]]
models = ["claude-*"]
credentials = ["b"]
"#,
        )
        .unwrap();
        assert_eq!(
            ns(&c).credentials_for("claude-opus-5", &c),
            vec!["a"],
            "個別指定が先"
        );
        assert_eq!(
            ns(&c).credentials_for("claude-sonnet-5", &c),
            vec!["b"],
            "後ろの広い規則"
        );
    }

    /// エイリアスは設定に書いたものだけ。コードに既定は無い。
    #[test]
    fn aliases_come_only_from_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(ns(&c).resolved_aliases()["o"], "claude-opus-*");

        let empty = parse("[ns.default]").unwrap();
        assert!(
            ns(&empty).resolved_aliases().is_empty(),
            "書かなければ 1 つも無い"
        );
    }

    #[test]
    fn aliases_are_read_as_written() {
        let c = parse(
            r#"
[ns.default.aliases]
opus = "claude-opus-4*"
"#,
        )
        .unwrap();
        assert_eq!(ns(&c).resolved_aliases()["opus"], "claude-opus-4*");
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

[[ns.default.routing]]
models = ["m"]
credentials = ["a", "typo-here"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo-here"), "{err}");
    }

    #[test]
    fn empty_routing_fields_are_rejected() {
        assert!(parse("[[ns.default.routing]]\nmodels = []\ncredentials = []").is_err());
        assert!(
            parse("[[ns.default.routing]]\nmodels = [\"m\"]\ncredentials = []")
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
        assert_eq!(ns(&again).routing.len(), ns(&original).routing.len());
        assert_eq!(ns(&again).filter.exclude, ns(&original).filter.exclude);
        assert_eq!(
            ns(&again).credentials_for("claude-fable-5", &again),
            ns(&original).credentials_for("claude-fable-5", &original)
        );
    }
}

#[cfg(test)]
mod example_tests {
    use super::tests::ns;
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
        assert!(!ns(&config).filter.exclude.is_empty(), "隠し方の例");
        assert!(!ns(&config).routing.is_empty(), "振り分けの例");
        assert!(!ns(&config).aliases.is_empty(), "短い名前の例");
        assert!(
            ns(&config).auth_token.is_some(),
            "雛形どおりに書けば認証がかかる (未設定は全拒否なので、書き方が要る)"
        );
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
