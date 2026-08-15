//! 設定ファイル。
//!
//! 既定の置き場は `$XDG_CONFIG_HOME/llm-gateway/config.toml`。`--config` で
//! 別の場所を指せる。
//!
//! ```toml
//! [server]
//! listen = "127.0.0.1:11300"
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
//! type = "bedrock_api_key"
//!
//! [routes.claude-personal]
//! provider = "anthropic"
//! credential = "claude-personal"
//!
//! [routes.bedrock]
//! provider = "anthropic"
//! credential = "bedrock"
//! url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"
//!
//! [routes.cpa]
//! provider = "anthropic"
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
//! routes = ["bedrock", "claude-personal"]
//!
//! [[ns.default.routing]]
//! models = ["gpt-*"]
//! routes = ["cpa"]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

mod extends;

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

    /// upstream へ出る経路。認証情報と話す API をここで組み合わせる。
    #[serde(default)]
    pub routes: BTreeMap<String, RouteSpec>,

    /// upstream への問い合わせ設定。
    ///
    /// これも共有。同じアカウントに namespace の数だけ聞きに行っても、
    /// 返ってくる一覧は同じ。
    #[serde(default)]
    pub discovery: Discovery,

    /// 起きたことの送り先 (DR-0012)。書かなければ送らない。
    #[serde(default)]
    pub webhook: Webhook,

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

    /// この namespace で使う経路。空なら全部。
    ///
    /// 面ごとに upstream を分けたいときに絞る。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,

    /// Messages API の思考表示方法を強制する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<ThinkingDisplay>,
}

/// Messages API の思考表示方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// 公開するモデルの絞り込み。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// 隠すモデル。`claude-opus-4*` のようにパターンで書ける。
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// 同じ優先度で試す経路。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RouteGroup {
    /// 1 経路だけのグループ。従来のフラットな記法。
    One(String),
    /// 7 日枠のリセットが近い順に試す同格の経路。
    Equal(Vec<String>),
}

impl RouteGroup {
    fn routes(&self) -> impl Iterator<Item = &str> {
        match self {
            Self::One(route) => std::slice::from_ref(route).iter(),
            Self::Equal(routes) => routes.iter(),
        }
        .map(String::as_str)
    }
}

/// 1 つの振り分け規則。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRule {
    /// 対象のモデル。パターンで書ける。
    pub models: Vec<String>,
    /// 使う経路を優先順に。内側の配列は同格の経路。
    pub routes: Vec<RouteGroup>,
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
    /// 待ち受け先。
    ///
    /// **この設定が指す面の住所**でもある。CLI (`usage` / `stats`) は、
    /// ここへ問い合わせる。
    #[serde(default = "default_listen")]
    pub listen: String,

    /// この設定では待ち受けない。
    ///
    /// `--config` を省いて CLI を使うための設定 (`config.toml`) を置きたい
    /// ことがある。共通部分を土台にして (DR-0013) `listen` だけ書いておけば、
    /// `llm-gateway usage` が問い合わせ先を知れる。ただしその設定で
    /// `serve` を始めると、既に動いている面と待ち受け先を取り合う。
    ///
    /// **`listen` の意味は変わらない** — 「どこで待つか」ではなく「どこに
    /// いるか」を書いた欄として、問い合わせ先の組み立てには使い続ける。
    #[serde(default)]
    pub disabled: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            disabled: false,
        }
    }
}

/// 既定の待ち受け先。
///
/// 11300 は小文字の `llm` の字形から 113、gateway らしく末尾を 00 にしたもの。
fn default_listen() -> String {
    "127.0.0.1:11300".to_owned()
}

/// 起きたことを送る先 (DR-0012)。
///
/// 見る側が繋ぎに来る形 (SSE) だと、待ち受けを複数並べたときに**掴んだ 1 つの
/// 面の分しか見えない**。こちらから送れば、面がいくつあっても同じ受け口に
/// 集まる。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// 受け口の根。ここに受け取る側が決めたパスを足した先へ送る。
    ///
    /// **書かなければ送らない** (機能ごと無効)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// 合言葉を置いたファイル。
    ///
    /// 設定に直接書かせないのは、設定ファイルが人の目に触れる場所 (共有した
    /// 土台・差分・貼り付けたログ) を通りやすいため。
    #[serde(default = "default_token_file")]
    pub token_file: PathBuf,
}

impl Default for Webhook {
    fn default() -> Self {
        Self {
            base_url: None,
            token_file: default_token_file(),
        }
    }
}

impl Webhook {
    /// 設定された根に、受け取る側が決めたパスを足す。
    pub fn destination_url(&self) -> std::result::Result<Option<url::Url>, String> {
        let Some(base) = &self.base_url else {
            return Ok(None);
        };
        let mut url =
            url::Url::parse(base).map_err(|e| format!("webhook.base_url is not a URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("webhook.base_url must be http or https".to_owned());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("webhook.base_url must not contain userinfo".to_owned());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("webhook.base_url must not contain a query or fragment".to_owned());
        }
        url.path_segments_mut()
            .map_err(|_| "webhook.base_url cannot be used as a base URL".to_owned())?
            .pop_if_empty()
            .push("webhook")
            .push("llm-gateway");
        Ok(Some(url))
    }
}

/// 既定の合言葉の置き場。受け取る側 (ccmsg) が置く場所に合わせる。
fn default_token_file() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
        .join("ccmsg")
        .join("webhook-llm-gateway.token")
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
/// 秘密そのものは store 側の `<key>.json` が持つ。ここで決めるのは payload の形と
/// login / refresh の手順だけで、接続先や話す API は [`RouteSpec`] が持つ。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialSpec {
    /// Anthropic のサブスク OAuth。
    ClaudeOauth,
    /// ChatGPT のサブスク OAuth。
    CodexOauth,
    /// Bedrock の API key。
    BedrockApiKey,
}

impl CredentialSpec {
    /// config.toml と保存 payload に共通する種別名。
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ClaudeOauth => "claude_oauth",
            Self::CodexOauth => "codex_oauth",
            Self::BedrockApiKey => "bedrock_api_key",
        }
    }
}

/// upstream へ出る 1 経路。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// 話す API。Auth は `credential` が指す認証情報の形から独立して選ぶ。
    pub provider: Provider,
    /// store から使う認証情報。転送先が認証を持つ経路では省略する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// 接続先。省略時は provider の既定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// upstream が受け付けない beta フラグ。Anthropic 方言だけが使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_beta: Option<Vec<String>>,
    /// discovery できない経路で公開するモデル。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// この経路で扱わないモデル。namespace の exclude に足して効く。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// upstream の API 方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Anthropic,
    Openai,
}

impl RouteSpec {
    pub fn url(&self) -> &str {
        self.url.as_deref().unwrap_or(match self.provider {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::Openai => "https://chatgpt.com/backend-api/codex",
        })
    }

    pub fn credential<'a>(&self, config: &'a Config) -> Option<&'a CredentialSpec> {
        self.credential
            .as_deref()
            .and_then(|name| config.credentials.get(name))
    }

    pub fn needs_secret(&self) -> bool {
        self.credential.is_some()
    }

    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    pub fn declared_models(&self) -> &[String] {
        &self.models
    }

    pub fn discovery_flavor(&self, config: &Config) -> Option<crate::discovery::Flavor> {
        match (self.provider, self.credential(config)) {
            (Provider::Anthropic, Some(CredentialSpec::ClaudeOauth)) => {
                Some(crate::discovery::Flavor::Anthropic)
            }
            (Provider::Anthropic, Some(CredentialSpec::BedrockApiKey)) => {
                Some(crate::discovery::Flavor::Bedrock)
            }
            (Provider::Openai, Some(CredentialSpec::CodexOauth)) => {
                Some(crate::discovery::Flavor::OpenAiCodex)
            }
            (Provider::Anthropic, None)
            | (Provider::Anthropic, Some(CredentialSpec::CodexOauth))
            | (Provider::Openai, _) => None,
        }
    }
}

fn validate_route(name: &str, route: &RouteSpec, config: &Config) -> Result<()> {
    let credential = route.credential(config);
    let supported = matches!(
        (route.provider, credential),
        (Provider::Anthropic, Some(CredentialSpec::ClaudeOauth))
            | (Provider::Anthropic, Some(CredentialSpec::BedrockApiKey))
            | (Provider::Anthropic, None)
            | (Provider::Openai, Some(CredentialSpec::CodexOauth))
    );
    if supported {
        return Ok(());
    }
    let credential_type = credential.map_or("none", CredentialSpec::type_name);
    Err(Error::Config(format!(
        "route `{name}` cannot combine provider `{:?}` with credential type `{credential_type}`",
        route.provider
    )))
}

impl Config {
    /// 読み込む。設定に矛盾があればここで弾く。
    pub fn load(path: &Path) -> Result<Self> {
        // 土台 (`extends`) があれば先に畳む (DR-0013)。
        let merged = extends::resolve(path)?;
        let config: Self = merged.try_into().map_err(|e| {
            Error::Config(format!(
                "{} is invalid: {e} (seen after merging with its base)",
                path.display()
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    /// 参照先が揃っているかを確かめる。
    ///
    /// 起動時に落としておかないと、その規則を最初に踏んだ人が
    /// 500 を見るまで誰も気づかない。
    pub fn validate(&self) -> Result<()> {
        self.webhook.destination_url().map_err(Error::Config)?;
        for (name, route) in &self.routes {
            if name.is_empty() {
                return Err(Error::Config("route name is empty".to_owned()));
            }
            if let Some(credential) = &route.credential
                && !self.credentials.contains_key(credential)
            {
                return Err(Error::Config(format!(
                    "route `{name}` references credential `{credential}`, which is not defined"
                )));
            }
            validate_route(name, route, self)?;
        }
        for (name, ns) in &self.namespaces {
            if name.is_empty() {
                return Err(Error::Config("namespace name is empty".to_owned()));
            }
            self.validate_namespace(name, ns)?;
        }
        Ok(())
    }

    fn validate_namespace(&self, ns_name: &str, ns: &Namespace) -> Result<()> {
        let known = |name: &String| self.routes.contains_key(name);

        for name in &ns.routes {
            if !known(name) {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` references route `{name}`, which is not defined"
                )));
            }
        }
        for (i, rule) in ns.routing.iter().enumerate() {
            if rule.models.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` routing[{i}] does not specify models"
                )));
            }
            if rule.routes.is_empty() {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` routing[{i}] ({}) does not specify routes",
                    rule.models.join(", ")
                )));
            }
            if rule
                .routes
                .iter()
                .any(|group| matches!(group, RouteGroup::Equal(routes) if routes.is_empty()))
            {
                return Err(Error::Config(format!(
                    "namespace `{ns_name}` routing[{i}] ({}) contains an empty route group",
                    rule.models.join(", ")
                )));
            }
            for name in rule.routes.iter().flat_map(RouteGroup::routes) {
                if !self.routes.contains_key(name) {
                    return Err(Error::Config(format!(
                        "namespace `{ns_name}` routing[{i}] ({}) references route `{name}`, which is not defined",
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

    /// このモデルを扱う経路を、試す順に返す。
    ///
    /// `routing` の上から照合し、最初に当たった規則を使う。当たらなければ
    /// この namespace が使える経路を宣言順に全部試す。
    pub fn routes_for<'a>(&'a self, model: &str, all: &'a Config) -> Vec<&'a str> {
        self.route_groups_for(model, all)
            .into_iter()
            .flatten()
            .collect()
    }

    /// このモデルを扱う経路を、同格グループの境界を保って返す。
    pub(crate) fn route_groups_for<'a>(
        &'a self,
        model: &str,
        all: &'a Config,
    ) -> Vec<Vec<&'a str>> {
        for rule in &self.routing {
            if crate::pattern::matches_any(&rule.models, model) {
                return rule
                    .routes
                    .iter()
                    .map(|group| group.routes().collect())
                    .collect();
            }
        }
        self.usable_routes(all)
            .into_iter()
            .map(|route| vec![route])
            .collect()
    }

    /// この namespace が使ってよい経路。
    pub fn usable_routes<'a>(&'a self, all: &'a Config) -> Vec<&'a str> {
        if self.routes.is_empty() {
            return all.routes.keys().map(String::as_str).collect();
        }
        self.routes.iter().map(String::as_str).collect()
    }

    /// この経路がこのモデルを扱ってよいか。
    pub fn allows(&self, route: &str, model: &str, all: &Config) -> bool {
        if crate::pattern::matches_any(&self.filter.exclude, model) {
            return false;
        }
        if !self.usable_routes(all).contains(&route) {
            return false;
        }
        match all.routes.get(route) {
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

    /// 土台を重ねた設定が、そのまま読み込めるところまで通る。
    ///
    /// 待ち受け先だけが違う 2 台を、同じ中身を書き写さずに用意できる
    /// (DR-0013)。畳んだ後の姿で項目名の検査まで通ることを、ここで見る。
    #[test]
    fn a_config_can_stand_on_another_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("shared.toml"),
            r#"
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:8320"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]

[ns.default]
auth_token = "t"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("unstable.toml"),
            "extends = \"shared.toml\"\n\n[server]\nlisten = \"127.0.0.1:8402\"\n",
        )
        .unwrap();

        let config = Config::load(&dir.path().join("unstable.toml")).unwrap();
        assert_eq!(
            config.server.listen, "127.0.0.1:8402",
            "what it wrote itself"
        );
        assert_eq!(
            config.namespace("default").unwrap().auth_token.as_deref(),
            Some("t"),
            "what comes from the base"
        );
        assert!(config.routes.contains_key("a"));
    }

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

[credentials.claude-work-b]
type = "claude_oauth"

[credentials.bedrock]
type = "bedrock_api_key"

[routes.claude-personal]
provider = "anthropic"
credential = "claude-personal"

[routes.claude-work-a]
provider = "anthropic"
credential = "claude-work-a"

# fable 専用のアカウント。
[routes.claude-work-b]
provider = "anthropic"
credential = "claude-work-b"
exclude = ["claude-opus-*", "claude-sonnet-*", "claude-haiku-*"]

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock-mantle.us-east-1.api.aws/anthropic"

[routes.cpa]
provider = "anthropic"
url = "http://127.0.0.1:8320"
models = ["gpt-*"]

# 古い世代は出さない。
[ns.default.filter]
exclude = ["claude-3-*", "claude-opus-4*", "claude-sonnet-4-*"]

# fable は Bedrock 優先 (Claude アカウントを消費しない)。
[[ns.default.routing]]
models = ["claude-fable-*"]
routes = ["bedrock", "claude-work-b", "claude-personal"]

[[ns.default.routing]]
models = ["gpt-*"]
routes = ["cpa"]

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
            .expect("this config has [ns.default]")
    }

    /// `thinking_display` は API が受け付ける 2 値だけを設定できる。
    ///
    /// 文字列のまま保持せず列挙型に閉じることで、不正値を起動前の設定解析で弾く。
    #[test]
    fn thinking_display_accepts_only_supported_values() {
        for (raw, expected) in [
            ("summarized", ThinkingDisplay::Summarized),
            ("omitted", ThinkingDisplay::Omitted),
        ] {
            let config = parse(&format!("[ns.default]\nthinking_display = \"{raw}\"\n"))
                .unwrap_or_else(|e| panic!("{raw} should be valid: {e}"));
            assert_eq!(ns(&config).thinking_display, Some(expected), "{raw}");
        }

        let error = parse("[ns.default]\nthinking_display = \"visible\"\n")
            .expect_err("an out-of-enum value is a config error");
        let message = error.to_string();
        assert!(message.contains("unknown variant `visible`"), "{message}");
        assert!(
            message.contains("summarized"),
            "lists the accepted values: {message}"
        );
        assert!(
            message.contains("omitted"),
            "lists the accepted values: {message}"
        );
    }

    /// `auth_token` を書かない namespace は、名乗り方に関わらず誰でも通す。
    ///
    /// 境界は手前 (tailnet / リバースプロキシ) で引く前提 (DR-0006)。
    #[test]
    fn namespace_without_token_accepts_everyone() {
        let ns = Namespace::default();
        assert_eq!(ns.auth_token, None, "the unwritten state");

        for presented in [None, Some("anything"), Some("Bearer anything"), Some("")] {
            assert_eq!(
                ns.authorize(presented),
                Authorization::Open,
                "{presented:?} passes too"
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
            "not identifying is treated the same as a mismatch"
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
            "only what was written is exposed"
        );
    }

    #[test]
    fn reads_a_full_config() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8317");
        assert_eq!(c.credentials.len(), 4);
        assert_eq!(c.routes.len(), 5);
        assert_eq!(ns(&c).routing.len(), 2);
        assert_eq!(c.discovery.refresh_secs, 3600, "default is 1 hour");
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
                "{hidden} is hidden"
            );
        }
        for shown in ["claude-opus-5", "claude-sonnet-5", "claude-fable-5"] {
            assert!(
                ns(&c).allows("claude-personal", shown, &c),
                "{shown} is shown"
            );
        }
    }

    /// credential ごとの exclude はその credential にだけ効く。
    #[test]
    fn per_credential_exclude_is_scoped() {
        let c = parse(SAMPLE).unwrap();
        assert!(
            !ns(&c).allows("claude-work-b", "claude-opus-5", &c),
            "fable-only, so opus is not handled"
        );
        assert!(
            ns(&c).allows("claude-work-b", "claude-fable-5", &c),
            "fable is handled"
        );
        assert!(
            ns(&c).allows("claude-personal", "claude-opus-5", &c),
            "has no effect on other credentials"
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
            ns(&c).routes_for("claude-fable-5", &c),
            vec!["bedrock", "claude-work-b", "claude-personal"]
        );
        assert_eq!(ns(&c).routes_for("gpt-5.6-sol", &c), vec!["cpa"]);
    }

    /// フラット配列は全 route が単独グループである従来記法として、そのまま読める。
    #[test]
    fn routing_accepts_flat_route_groups() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).route_groups_for("claude-fable-5", &c),
            vec![
                vec!["bedrock"],
                vec!["claude-work-b"],
                vec!["claude-personal"]
            ]
        );
    }

    /// 内側の配列は同格グループで、外側の文字列は単独グループとして境界を保つ。
    #[test]
    fn routing_accepts_equal_route_groups() {
        let c = parse(
            r#"
[credentials.a]
type = "claude_oauth"
[credentials.b]
type = "claude_oauth"
[credentials.c]
type = "claude_oauth"
[routes.a]
provider = "anthropic"
credential = "a"
[routes.b]
provider = "anthropic"
credential = "b"
[routes.c]
provider = "anthropic"
credential = "c"
[[ns.default.routing]]
models = ["model"]
routes = [["a", "b"], "c"]
"#,
        )
        .unwrap();
        assert_eq!(
            ns(&c).route_groups_for("model", &c),
            vec![vec!["a", "b"], vec!["c"]]
        );
    }

    /// グループの中にグループは書けない。ネストは同格を表す 1 段だけに限定する。
    #[test]
    fn routing_rejects_groups_nested_two_levels() {
        let err = parse(
            r#"
[[ns.default.routing]]
models = ["model"]
routes = [[["a"]]]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("routes"), "{err}");
    }

    /// 空の同格グループは候補を持たず設定ミスを隠すため、起動時に拒否する。
    #[test]
    fn routing_rejects_empty_route_groups() {
        let err = parse(
            r#"
[[ns.default.routing]]
models = ["model"]
routes = [[]]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty route group"), "{err}");
    }

    /// 規則に当たらないモデルは宣言順に試す。
    /// 新しいモデルが出ても、設定を触らずに使える。
    #[test]
    fn unmatched_models_fall_back_to_declaration_order() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(
            ns(&c).routes_for("claude-opus-6", &c),
            vec![
                "bedrock",
                "claude-personal",
                "claude-work-a",
                "claude-work-b",
                "cpa"
            ],
            "declaration order (name order, since it's a BTreeMap)"
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

[routes.a]
provider = "anthropic"
credential = "a"

[routes.b]
provider = "anthropic"
credential = "b"

[[ns.default.routing]]
models = ["claude-opus-5"]
routes = ["a"]

[[ns.default.routing]]
models = ["claude-*"]
routes = ["b"]
"#,
        )
        .unwrap();
        assert_eq!(
            ns(&c).routes_for("claude-opus-5", &c),
            vec!["a"],
            "a specific entry comes first"
        );
        assert_eq!(
            ns(&c).routes_for("claude-sonnet-5", &c),
            vec!["b"],
            "a broader rule comes after"
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
            "none at all when unwritten"
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
            c.routes["claude-personal"].discovery_flavor(&c),
            Some(Flavor::Anthropic)
        );
        assert_eq!(
            c.routes["bedrock"].discovery_flavor(&c),
            Some(Flavor::Bedrock)
        );
        assert_eq!(
            c.routes["cpa"].discovery_flavor(&c),
            None,
            "cannot be queried on a relay target"
        );
    }

    /// 聞けない upstream は設定に書いたモデルを使う。
    #[test]
    fn relay_declares_its_models() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.routes["cpa"].declared_models(), ["gpt-*"]);
        assert!(c.routes["claude-personal"].declared_models().is_empty());
    }

    #[test]
    fn unknown_credential_reference_is_rejected() {
        let err = parse(
            r#"
[credentials.a]
type = "claude_oauth"

[routes.a]
provider = "anthropic"
credential = "a"

[[ns.default.routing]]
models = ["m"]
routes = ["a", "typo-here"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo-here"), "{err}");
    }

    #[test]
    fn empty_routing_fields_are_rejected() {
        assert!(parse("[[ns.default.routing]]\nmodels = []\nroutes = []").is_err());
        assert!(
            parse("[[ns.default.routing]]\nmodels = [\"m\"]\nroutes = []")
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
        assert_eq!(
            c.server.listen, "127.0.0.1:11300",
            "the default listen address"
        );
        assert!(!c.server.disabled, "listens when unwritten");
        assert!(matches!(c.store, Store::File { dir: None }));
    }

    /// 待ち受けないと書ける。住所 (`listen`) は書いても書かなくてもよい。
    ///
    /// `--config` を省いて CLI を使うための設定に使う (DR-0013)。
    #[test]
    fn a_config_can_declare_that_it_does_not_listen() {
        let c = parse("[server]\ndisabled = true\nlisten = \"127.0.0.1:8402\"").unwrap();
        assert!(c.server.disabled);
        assert_eq!(
            c.server.listen, "127.0.0.1:8402",
            "still remains as a queryable target"
        );
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
    fn webhook_is_disabled_by_default() {
        let c = parse("").unwrap();
        assert!(c.webhook.base_url.is_none());
        assert!(
            c.webhook
                .token_file
                .ends_with("ccmsg/webhook-llm-gateway.token")
        );
    }

    #[test]
    fn webhook_destination_and_token_file_can_be_set() {
        let c = parse(
            "[webhook]\nbase_url = \"http://127.0.0.1:1234/\"\ntoken_file = \"/tmp/hook.token\"",
        )
        .unwrap();
        assert_eq!(
            c.webhook.base_url.as_deref(),
            Some("http://127.0.0.1:1234/")
        );
        assert_eq!(c.webhook.token_file, PathBuf::from("/tmp/hook.token"));
        assert_eq!(
            c.webhook.destination_url().unwrap().unwrap().as_str(),
            "http://127.0.0.1:1234/webhook/llm-gateway"
        );
    }

    #[test]
    fn webhook_base_path_is_preserved() {
        let c = parse("[webhook]\nbase_url = \"https://example.test/hooks\"").unwrap();
        assert_eq!(
            c.webhook.destination_url().unwrap().unwrap().as_str(),
            "https://example.test/hooks/webhook/llm-gateway"
        );
    }

    #[test]
    fn unsafe_webhook_destinations_are_rejected() {
        for base in [
            "",
            "file:///tmp/hook",
            "https://user@example.test",
            "https://example.test?target=other",
            "https://example.test#other",
        ] {
            let source = format!("[webhook]\nbase_url = {base:?}");
            assert!(parse(&source).is_err(), "{base} was accepted");
        }
    }

    #[test]
    fn unknown_webhook_key_is_rejected() {
        let err = parse("[webhook]\nurl = \"http://127.0.0.1:1234\"").unwrap_err();
        assert!(err.to_string().contains("url"), "{err}");
    }

    #[test]
    fn round_trips_through_toml() {
        let original = parse(SAMPLE).unwrap();
        let again = parse(&toml::to_string(&original).unwrap()).unwrap();
        assert_eq!(ns(&again).routing.len(), ns(&original).routing.len());
        assert_eq!(ns(&again).filter.exclude, ns(&original).filter.exclude);
        assert_eq!(
            ns(&again).routes_for("claude-fable-5", &again),
            ns(&original).routes_for("claude-fable-5", &original)
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
        let raw = std::fs::read_to_string(path).expect("dist/config.example.toml is required");
        let config: Config = toml::from_str(&raw).expect("the template is broken");
        config
            .validate()
            .expect("a reference in the template is broken");

        // 使い方を示す要素が一通り入っているか。
        assert!(
            !ns(&config).filter.exclude.is_empty(),
            "an example of hiding"
        );
        assert!(!ns(&config).routing.is_empty(), "an example of routing");
        assert!(
            !ns(&config).aliases.is_empty(),
            "an example of a short name"
        );
        assert!(
            ns(&config).auth_token.is_some(),
            "following the template as-is enables auth (unset means deny-all, so the example is needed)"
        );
        assert!(
            config
                .routes
                .values()
                .any(|route| !route.exclude().is_empty()),
            "an example of a per-credential restriction"
        );
        assert!(
            config
                .routes
                .values()
                .any(|route| !route.declared_models().is_empty()),
            "an example of declaring the models a relay handles"
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
        assert!(
            !raw.contains("[models."),
            "a legacy-format model definition remains"
        );
    }
}
