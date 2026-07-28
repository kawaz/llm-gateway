//! どのモデルをどの経路へ送るかを決める。
//!
//! 公開するモデルは upstream に聞いて集める。手で並べると新しいモデルが出る
//! たびに追記が要るし、書き忘れたものは 404 になる。
//!
//! 同じ会話は同じ経路に貼り続ける。貼り直すと prompt cache が無駄になり、
//! upstream から見てアカウントをまたいだ利用にも見える。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::backend::anthropic::{Bedrock, Official, Provider, Relay};
use crate::config::{Config, CredentialSpec};
use crate::credential::{Credential, CredentialId, CredentialStore, Persistence};
use crate::discovery::{self, Model};
use crate::session::SessionKey;
use crate::{Error, Result};

/// 会話と経路の結びつきを保つ時間。
///
/// 短いと同じ会話が別の経路へ飛んで prompt cache を捨てることになり、
/// 長いと落ちた経路に貼り付いたままになる。
const AFFINITY_TTL: Duration = Duration::from_secs(3600);

/// 1 経路。どの upstream へ、どの認証情報で送るか。
pub struct Route {
    pub provider: Arc<dyn Provider>,
    /// 転送先が認証を持つ経路 (relay) では要らない。
    pub credential: Option<CredentialId>,
}

impl Route {
    /// ログや失敗記録に出す名前。
    pub fn name(&self) -> &str {
        self.provider.name()
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("provider", &self.provider.name())
            .field("credential", &self.credential)
            .finish()
    }
}

/// upstream に聞いて分かったこと。
#[derive(Default)]
struct Catalog {
    /// 公開するモデル名 → それを扱える credential 名 (宣言順)。
    models: BTreeMap<String, Vec<String>>,
    /// credential ごとの「クライアント名 → upstream 名」。
    /// Bedrock は名前空間が付くので変換が要る。
    upstream_names: HashMap<String, BTreeMap<String, String>>,
    /// 短い名前 → 実際のモデル名。
    aliases: BTreeMap<String, String>,
}

pub struct Router {
    config: Config,
    catalog: RwLock<Catalog>,
    affinity: Mutex<HashMap<SessionKey, Binding>>,
}

struct Binding {
    route: Arc<Route>,
    /// 同じモデルの会話にだけ効かせる。モデルが変われば選び直す。
    model: String,
    seen: Instant,
}

impl Router {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            catalog: RwLock::new(Catalog::default()),
            affinity: Mutex::new(HashMap::new()),
        }
    }

    /// upstream に一覧を聞いて、公開するモデルを組み直す。
    ///
    /// 聞けなかった upstream は前回の結果を残す。一時的に繋がらないだけで
    /// 公開しているモデルが消えるのは困る。
    pub async fn refresh<P: Persistence>(
        &self,
        http: &reqwest::Client,
        credentials: &CredentialStore<P>,
    ) {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut upstream_names: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        let previous = self.catalog.read().await;

        for (name, spec) in &self.config.credentials {
            let found = match spec.discovery_flavor() {
                Some(flavor) => {
                    match self.discover(http, credentials, name, spec, flavor).await {
                        Ok(found) => found,
                        Err(e) => {
                            warn!(credential = %name, %e, "一覧を取れません。前回の結果を使います");
                            // 前回の結果から、この credential の分を復元する。
                            previous
                                .upstream_names
                                .get(name)
                                .map(|m| {
                                    m.iter()
                                        .map(|(id, upstream)| Model {
                                            id: id.clone(),
                                            upstream_id: upstream.clone(),
                                            created: 0,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        }
                    }
                }
                // 聞けない upstream は設定に書かれたものを使う。
                None => spec
                    .declared_models()
                    .iter()
                    .map(|pattern| Model {
                        id: pattern.clone(),
                        upstream_id: pattern.clone(),
                        created: 0,
                    })
                    .collect(),
            };

            let mut mapping = BTreeMap::new();
            for m in found {
                if !self.config.allows(name, &m.id) {
                    continue;
                }
                mapping.insert(m.id.clone(), m.upstream_id.clone());
                models.entry(m.id).or_default().push(name.clone());
            }
            upstream_names.insert(name.clone(), mapping);
        }
        drop(previous);

        // エイリアスは、集まった一覧の中から一番新しいものへ向ける。
        let known: Vec<Model> = models
            .keys()
            .map(|id| Model {
                id: id.clone(),
                upstream_id: id.clone(),
                created: 0,
            })
            .collect();
        let aliases = self
            .config
            .resolved_aliases()
            .into_iter()
            .filter_map(|(alias, pattern)| {
                discovery::resolve_alias(&pattern, &known).map(|target| (alias, target))
            })
            .collect();

        info!(models = models.len(), "モデル一覧を更新しました");
        *self.catalog.write().await = Catalog {
            models,
            upstream_names,
            aliases,
        };
    }

    async fn discover<P: Persistence>(
        &self,
        http: &reqwest::Client,
        credentials: &CredentialStore<P>,
        name: &str,
        spec: &CredentialSpec,
        flavor: discovery::Flavor,
    ) -> Result<Vec<Model>> {
        let credential = credentials.acquire(&CredentialId::new(name)).await?;
        let mut found = discovery::fetch(http, flavor, spec.url(), &credential).await?;
        // 日付は resolve_alias が使う。取れないものは 0 のまま。
        found.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(found)
    }

    /// 公開しているモデル名 (エイリアスを含む)。
    pub async fn models(&self) -> Vec<String> {
        let catalog = self.catalog.read().await;
        let mut all: Vec<String> = catalog.models.keys().cloned().collect();
        all.extend(catalog.aliases.keys().cloned());
        all.sort_unstable();
        all.dedup();
        all
    }

    /// エイリアスなら実際のモデル名に直す。
    pub async fn resolve(&self, model: &str) -> String {
        let catalog = self.catalog.read().await;
        catalog
            .aliases
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned())
    }

    /// このモデルを実際に試す順。表示用。
    ///
    /// 設定の優先順そのままではなく、**そのモデルを扱える credential だけ**に
    /// 絞ったもの。設定を見ただけでは分からないので、確認に使える。
    pub async fn route_names(&self, model: &str) -> Vec<String> {
        let model = self.resolve(model).await;
        let catalog = self.catalog.read().await;
        let Some(available) = catalog.models.get(&model) else {
            return Vec::new();
        };
        self.config
            .credentials_for(&model)
            .into_iter()
            .filter(|name| available.iter().any(|a| a == name))
            .map(str::to_owned)
            .collect()
    }

    /// この会話でこのモデルを使うときの経路を、試す順に返す。
    pub async fn routes_for(&self, model: &str, session: &SessionKey) -> Result<Vec<Arc<Route>>> {
        let catalog = self.catalog.read().await;
        let available = catalog
            .models
            .get(model)
            .ok_or_else(|| Error::UnknownModel(model.to_owned()))?;

        // 設定の優先順のうち、このモデルを実際に扱えるものだけを残す。
        let ordered: Vec<&str> = self
            .config
            .credentials_for(model)
            .into_iter()
            .filter(|name| available.iter().any(|a| a == name))
            .collect();

        let mut routes: Vec<Arc<Route>> = ordered
            .iter()
            .filter_map(|name| self.build_route(&catalog, name, model))
            .collect();
        drop(catalog);

        if routes.is_empty() {
            return Err(Error::UnknownModel(model.to_owned()));
        }

        // 前回通った経路を先頭へ。落ちていても他を試せるよう、候補は減らさない。
        let mut affinity = self.affinity.lock().await;
        let now = Instant::now();
        affinity.retain(|_, b| now.duration_since(b.seen) < AFFINITY_TTL);

        if let Some(bound) = affinity.get(session).filter(|b| b.model == model)
            && let Some(at) = routes.iter().position(|r| r.name() == bound.route.name())
        {
            routes.swap(0, at);
        }
        Ok(routes)
    }

    fn build_route(&self, catalog: &Catalog, name: &str, model: &str) -> Option<Arc<Route>> {
        let spec = self.config.credentials.get(name)?;
        let upstream_name = catalog
            .upstream_names
            .get(name)
            .and_then(|m| m.get(model))
            .cloned();

        // upstream での名前が違う場合だけ書き換える。
        let model_map = match &upstream_name {
            Some(upstream) if upstream != model => {
                BTreeMap::from([(model.to_owned(), upstream.clone())])
            }
            _ => BTreeMap::new(),
        };

        let provider: Arc<dyn Provider> = match spec {
            CredentialSpec::ClaudeOauth { url, headers, .. } => {
                Arc::new(Official::new(name, url, headers.clone()))
            }
            CredentialSpec::ClaudeBedrock {
                url,
                headers,
                deny_beta,
                ..
            } => Arc::new(Bedrock::new(
                name,
                url,
                headers.clone(),
                deny_beta.clone(),
                model_map,
            )),
            // Responses API への変換は未実装。それまでは転送で凌ぐ。
            CredentialSpec::CodexOauth { url, headers, .. }
            | CredentialSpec::Relay { url, headers, .. } => {
                Arc::new(Relay::new(name, url, headers.clone()))
            }
        };

        Some(Arc::new(Route {
            provider,
            credential: spec.needs_secret().then(|| CredentialId::new(name)),
        }))
    }

    /// 実際に使えた経路を覚える。
    pub async fn remember(&self, session: &SessionKey, model: &str, route: &Arc<Route>) {
        self.affinity.lock().await.insert(
            session.clone(),
            Binding {
                route: Arc::clone(route),
                model: model.to_owned(),
                seen: Instant::now(),
            },
        );
    }

    #[cfg(test)]
    async fn set_catalog(&self, models: &[(&str, &[&str])], upstream: &[(&str, &[(&str, &str)])]) {
        let mut catalog = self.catalog.write().await;
        catalog.models = models
            .iter()
            .map(|(m, creds)| {
                (
                    (*m).to_owned(),
                    creds.iter().map(|c| (*c).to_owned()).collect(),
                )
            })
            .collect();
        catalog.upstream_names = upstream
            .iter()
            .map(|(cred, pairs)| {
                (
                    (*cred).to_owned(),
                    pairs
                        .iter()
                        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                        .collect(),
                )
            })
            .collect();
        let known: Vec<Model> = catalog
            .models
            .keys()
            .map(|id| Model {
                id: id.clone(),
                upstream_id: id.clone(),
                created: 0,
            })
            .collect();
        catalog.aliases = self
            .config
            .resolved_aliases()
            .into_iter()
            .filter_map(|(a, p)| discovery::resolve_alias(&p, &known).map(|t| (a, t)))
            .collect();
    }
}

/// 認証情報を渡す先。gateway が持つ store をそのまま使う。
pub type Credentials<'a> = &'a dyn Fn(&CredentialId) -> Option<Credential>;

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[filter]
exclude = ["claude-opus-4*"]

[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock.invalid/anthropic"

[credentials.oauth-a]
type = "claude_oauth"

[credentials.oauth-b]
type = "claude_oauth"
exclude = ["claude-haiku-*"]

[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:8320"
models = ["gpt-5.6-sol"]

[[routing]]
models = ["claude-fable-*"]
credentials = ["bedrock", "oauth-a"]

[[routing]]
models = ["gpt-*"]
credentials = ["cpa"]

[aliases]
fable = "claude-fable-*"
opus = "claude-opus-*"
haiku = "claude-haiku-*"
"#;

    /// discovery 済みの状態を作る。
    async fn router() -> Router {
        let config: Config = toml::from_str(CONFIG).unwrap();
        config.validate().unwrap();
        let r = Router::new(config);
        r.set_catalog(
            &[
                ("claude-fable-5", &["bedrock", "oauth-a"]),
                ("claude-opus-5", &["oauth-a", "oauth-b"]),
                ("claude-haiku-4-5-20251001", &["oauth-a"]),
                ("gpt-5.6-sol", &["cpa"]),
            ],
            &[
                ("bedrock", &[("claude-fable-5", "anthropic.claude-fable-5")]),
                ("oauth-a", &[("claude-fable-5", "claude-fable-5")]),
            ],
        )
        .await;
        r
    }

    fn session(name: &str) -> SessionKey {
        crate::session::derive(&serde_json::json!({"conversation_id": name}), &[])
    }

    fn names(routes: &[Arc<Route>]) -> Vec<&str> {
        routes.iter().map(|r| r.name()).collect()
    }

    #[tokio::test]
    async fn follows_routing_rules() {
        let r = router().await;
        let got = r
            .routes_for("claude-fable-5", &session("s1"))
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a"]);
    }

    /// 規則に無いモデルは、扱える credential を宣言順に試す。
    /// 新しいモデルが出ても設定を触らずに使える。
    #[tokio::test]
    async fn unrouted_model_uses_whoever_can_serve_it() {
        let r = router().await;
        let got = r.routes_for("claude-opus-5", &session("s1")).await.unwrap();
        assert_eq!(names(&got), vec!["oauth-a", "oauth-b"]);
    }

    /// そのモデルを扱えない credential は候補に入らない。
    #[tokio::test]
    async fn excluded_credential_is_not_offered() {
        let r = router().await;
        let got = r
            .routes_for("claude-haiku-4-5-20251001", &session("s1"))
            .await
            .unwrap();
        assert_eq!(
            names(&got),
            vec!["oauth-a"],
            "oauth-b は haiku を除外している"
        );
    }

    #[tokio::test]
    async fn unknown_model_is_rejected() {
        let r = router().await;
        let err = r
            .routes_for("no-such-model", &session("s1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no-such-model"), "{err}");
    }

    /// upstream で名前が違うものだけ書き換える。
    #[tokio::test]
    async fn rewrites_model_name_only_where_needed() {
        use crate::backend::anthropic::Headers;
        use serde_json::json;

        let r = router().await;
        let routes = r
            .routes_for("claude-fable-5", &session("s1"))
            .await
            .unwrap();

        let mut body = json!({"model": "claude-fable-5"});
        routes[0].provider.adapt(&mut body, &mut Headers::default());
        assert_eq!(
            body["model"], "anthropic.claude-fable-5",
            "Bedrock は名前空間つき"
        );

        let mut body = json!({"model": "claude-fable-5"});
        routes[1].provider.adapt(&mut body, &mut Headers::default());
        assert_eq!(body["model"], "claude-fable-5", "公式はそのまま");
    }

    /// エイリアスは一番新しいものに向く。
    #[tokio::test]
    async fn aliases_resolve_to_concrete_models() {
        let r = router().await;
        assert_eq!(r.resolve("opus").await, "claude-opus-5");
        assert_eq!(r.resolve("fable").await, "claude-fable-5");
        assert_eq!(r.resolve("haiku").await, "claude-haiku-4-5-20251001");
    }

    /// エイリアスでない名前はそのまま通す。
    #[tokio::test]
    async fn non_alias_passes_through() {
        let r = router().await;
        assert_eq!(r.resolve("claude-opus-5").await, "claude-opus-5");
        assert_eq!(r.resolve("unknown").await, "unknown");
    }

    /// 一覧にはエイリアスも並べる。短い名前で選べるようにする。
    #[tokio::test]
    async fn model_list_includes_aliases() {
        let r = router().await;
        let models = r.models().await;
        assert!(models.contains(&"claude-opus-5".to_owned()));
        assert!(models.contains(&"opus".to_owned()));
        assert!(models.contains(&"fable".to_owned()));
        // sonnet は catalog に無いので、エイリアスも出ない。
        assert!(!models.contains(&"sonnet".to_owned()));
    }

    /// 一度通った経路を次も先に試す。
    #[tokio::test]
    async fn sticks_to_the_route_that_worked() {
        let r = router().await;
        let s = session("s1");

        let first = r.routes_for("claude-fable-5", &s).await.unwrap();
        r.remember(&s, "claude-fable-5", &first[1]).await;

        let again = r.routes_for("claude-fable-5", &s).await.unwrap();
        assert_eq!(names(&again), vec!["oauth-a", "bedrock"]);
        assert_eq!(again.len(), 2, "候補は減らさない");
    }

    #[tokio::test]
    async fn bindings_are_per_session_and_model() {
        let r = router().await;
        let s = session("s1");

        let routes = r.routes_for("claude-fable-5", &s).await.unwrap();
        r.remember(&s, "claude-fable-5", &routes[1]).await;

        assert_eq!(
            names(
                &r.routes_for("claude-fable-5", &session("s2"))
                    .await
                    .unwrap()
            ),
            vec!["bedrock", "oauth-a"],
            "別の会話には効かない"
        );
        assert_eq!(
            names(&r.routes_for("claude-opus-5", &s).await.unwrap()),
            vec!["oauth-a", "oauth-b"],
            "別のモデルには効かない"
        );
    }

    /// 表示用の経路は、実際に試すものと一致する。
    ///
    /// 設定の優先順をそのまま出すと、そのモデルを扱えない credential まで
    /// 並んで実態と食い違う。
    #[tokio::test]
    async fn route_names_match_what_is_actually_tried() {
        let r = router().await;
        let s = session("s1");

        for model in [
            "claude-fable-5",
            "claude-opus-5",
            "claude-haiku-4-5-20251001",
        ] {
            let actual: Vec<String> = r
                .routes_for(model, &s)
                .await
                .unwrap()
                .iter()
                .map(|route| route.name().to_owned())
                .collect();
            assert_eq!(r.route_names(model).await, actual, "{model}");
        }
    }

    /// エイリアスでも実際の経路を出す。
    #[tokio::test]
    async fn route_names_resolves_aliases() {
        let r = router().await;
        assert_eq!(
            r.route_names("fable").await,
            r.route_names("claude-fable-5").await
        );
    }

    #[tokio::test]
    async fn route_names_is_empty_for_unknown_model() {
        let r = router().await;
        assert!(r.route_names("no-such-model").await.is_empty());
    }

    /// 一覧が空なら何も出さない (起動直後で discovery 前の状態)。
    #[tokio::test]
    async fn empty_catalog_serves_nothing() {
        let config: Config = toml::from_str(CONFIG).unwrap();
        let r = Router::new(config);
        assert!(r.models().await.is_empty());
        assert!(r.routes_for("claude-opus-5", &session("s")).await.is_err());
    }
}
