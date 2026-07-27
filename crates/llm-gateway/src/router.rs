//! モデル名から経路を選ぶ。
//!
//! 1 つのモデルに複数の経路を優先順で持たせ、上から試す。fable-5 なら
//! 通常は Bedrock (Claude アカウントを消費しない) を使い、そこが落ちている
//! 間だけ OAuth プールへ回す、といった指定ができる。
//!
//! 同じ会話は同じ経路に貼り続ける。貼り直すと prompt cache が無駄になり、
//! upstream から見てアカウントをまたいだ利用にも見える。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::backend::anthropic::Provider;
use crate::config::{Config, CredentialSpec};
use crate::credential::CredentialId;
use crate::session::SessionKey;
use crate::{Error, Result};

/// 会話と経路の結びつきを保つ時間。
///
/// 短いと同じ会話が別の経路へ飛んで prompt cache を捨てることになり、
/// 長いと落ちた経路に貼り付いたままになる。cpa の既定と揃えて 1 時間。
const AFFINITY_TTL: Duration = Duration::from_secs(3600);

/// 1 経路。どの upstream へ、どの認証情報で送るか。
pub struct Route {
    pub provider: Arc<dyn Provider>,
    /// `relay` のように認証情報を持たない経路もある。
    pub credential: Option<CredentialId>,
}

impl Route {
    fn name(&self) -> &str {
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

pub struct Router {
    /// クライアントが指定するモデル名 → 優先順に並んだ経路。
    routes: HashMap<String, Vec<Arc<Route>>>,
    /// 会話 → 前回選んだ経路。
    affinity: Mutex<HashMap<SessionKey, Binding>>,
}

struct Binding {
    route: Arc<Route>,
    /// 同じモデルの会話にだけ効かせる。モデルが変われば選び直す。
    model: String,
    seen: Instant,
}

impl Router {
    /// 設定から組み立てる。
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut routes = HashMap::new();

        for (model, route_config) in &config.models {
            let mut candidates = Vec::new();
            for name in &route_config.credentials {
                let spec = config.credentials.get(name).ok_or_else(|| {
                    Error::Config(format!(
                        "model `{model}` が参照する credential `{name}` が定義されていません"
                    ))
                })?;
                candidates.push(Arc::new(Route {
                    provider: build_provider(
                        name,
                        spec,
                        model,
                        route_config.upstream_name.as_deref(),
                    ),
                    credential: spec.needs_secret().then(|| CredentialId::new(name)),
                }));
            }
            routes.insert(model.clone(), candidates);
        }

        Ok(Self {
            routes,
            affinity: Mutex::new(HashMap::new()),
        })
    }

    /// 公開しているモデル名。
    pub fn models(&self) -> impl Iterator<Item = &str> {
        self.routes.keys().map(String::as_str)
    }

    /// この会話でこのモデルを使うときの経路を、試す順に返す。
    ///
    /// 前回と同じ経路を先頭に置く。それ以外は設定の優先順のまま後ろに並べ、
    /// 前回の経路が落ちていれば次へ回れるようにする。
    pub async fn routes_for(&self, model: &str, session: &SessionKey) -> Result<Vec<Arc<Route>>> {
        let configured = self
            .routes
            .get(model)
            .ok_or_else(|| Error::UnknownModel(model.to_owned()))?;

        let mut affinity = self.affinity.lock().await;
        self.evict_stale(&mut affinity);

        let bound = affinity
            .get(session)
            .filter(|b| b.model == model)
            .map(|b| Arc::clone(&b.route));

        let Some(bound) = bound else {
            return Ok(configured.clone());
        };

        let mut ordered = vec![Arc::clone(&bound)];
        ordered.extend(
            configured
                .iter()
                .filter(|r| !Arc::ptr_eq(r, &bound))
                .cloned(),
        );
        Ok(ordered)
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

    /// 使われなくなった結びつきを捨てる。
    ///
    /// 放っておくと会話の数だけ溜まる。常駐なので、掃除しないと増え続ける。
    fn evict_stale(&self, affinity: &mut HashMap<SessionKey, Binding>) {
        let now = Instant::now();
        affinity.retain(|_, b| now.duration_since(b.seen) < AFFINITY_TTL);
    }

    #[cfg(test)]
    async fn affinity_len(&self) -> usize {
        self.affinity.lock().await.len()
    }
}

fn build_provider(
    name: &str,
    spec: &CredentialSpec,
    model: &str,
    upstream_name: Option<&str>,
) -> Arc<dyn Provider> {
    use crate::backend::anthropic::{Bedrock, Official, Relay};
    use std::collections::BTreeMap;

    match spec {
        CredentialSpec::ClaudeOauth { url, headers } => {
            Arc::new(Official::new(name, url, headers.clone()))
        }
        CredentialSpec::ClaudeBedrock {
            url,
            headers,
            deny_beta,
        } => {
            // モデル名の対応はこの経路にだけ効かせる。公式へ回ったときに
            // Bedrock の名前で送ると 404 になる。
            let model_map = match upstream_name {
                Some(upstream) => BTreeMap::from([(model.to_owned(), upstream.to_owned())]),
                None => BTreeMap::new(),
            };
            Arc::new(Bedrock::new(
                name,
                url,
                headers.clone(),
                deny_beta.clone(),
                model_map,
            ))
        }
        // Responses API を話す経路は Phase 2。それまでは relay 扱いにして
        // 別の gateway へ渡す。
        CredentialSpec::CodexOauth { url, headers } | CredentialSpec::Relay { url, headers } => {
            Arc::new(Relay::new(name, url, headers.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock.invalid/anthropic"

[credentials.oauth-a]
type = "claude_oauth"

[credentials.oauth-b]
type = "claude_oauth"

[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:8317"

[models."claude-fable-5"]
upstream_name = "anthropic.claude-fable-5"
credentials = ["bedrock", "oauth-a"]

[models."claude-opus-5"]
credentials = ["oauth-a", "oauth-b"]

[models."gpt-5.6-sol"]
credentials = ["cpa"]
"#;

    fn router() -> Router {
        let config: Config = toml::from_str(CONFIG).unwrap();
        config.validate().unwrap();
        Router::from_config(&config).unwrap()
    }

    fn session(name: &str) -> SessionKey {
        crate::session::derive(&serde_json::json!({"conversation_id": name}), &[])
    }

    fn names(routes: &[Arc<Route>]) -> Vec<&str> {
        routes.iter().map(|r| r.name()).collect()
    }

    #[tokio::test]
    async fn follows_configured_priority() {
        let r = router();
        let got = r
            .routes_for("claude-fable-5", &session("s1"))
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a"]);
    }

    #[tokio::test]
    async fn unknown_model_is_rejected() {
        let r = router();
        let err = r
            .routes_for("no-such-model", &session("s1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no-such-model"), "{err}");
    }

    /// 認証情報が要る経路と要らない経路がある。
    #[tokio::test]
    async fn relay_route_has_no_credential() {
        let r = router();
        let got = r.routes_for("gpt-5.6-sol", &session("s1")).await.unwrap();
        assert!(got[0].credential.is_none(), "転送先が認証を持つ");

        let got = r.routes_for("claude-opus-5", &session("s1")).await.unwrap();
        assert_eq!(
            got[0].credential.as_ref().map(CredentialId::as_str),
            Some("oauth-a")
        );
    }

    /// 一度使えた経路を次回も先に試す。
    #[tokio::test]
    async fn sticks_to_the_route_that_worked() {
        let r = router();
        let s = session("s1");

        let first = r.routes_for("claude-opus-5", &s).await.unwrap();
        // 先頭が落ちていて 2 番目が通った、という状況。
        r.remember(&s, "claude-opus-5", &first[1]).await;

        let again = r.routes_for("claude-opus-5", &s).await.unwrap();
        assert_eq!(
            names(&again),
            vec!["oauth-b", "oauth-a"],
            "通った方を先に試す"
        );
    }

    /// 貼り付いた先が落ちても、残りを試せる。
    #[tokio::test]
    async fn bound_route_does_not_hide_the_others() {
        let r = router();
        let s = session("s1");
        let routes = r.routes_for("claude-fable-5", &s).await.unwrap();
        r.remember(&s, "claude-fable-5", &routes[1]).await;

        let again = r.routes_for("claude-fable-5", &s).await.unwrap();
        assert_eq!(again.len(), 2, "候補が減らない");
        assert_eq!(names(&again), vec!["oauth-a", "bedrock"]);
    }

    /// 会話が違えば別々に貼り付く。
    #[tokio::test]
    async fn bindings_are_per_session() {
        let r = router();
        let (s1, s2) = (session("s1"), session("s2"));

        let routes = r.routes_for("claude-opus-5", &s1).await.unwrap();
        r.remember(&s1, "claude-opus-5", &routes[1]).await;

        assert_eq!(
            names(&r.routes_for("claude-opus-5", &s2).await.unwrap()),
            vec!["oauth-a", "oauth-b"],
            "別の会話は設定どおりの順"
        );
    }

    /// 同じ会話でもモデルが変われば選び直す。
    ///
    /// fable-5 で Bedrock に貼り付いていても、opus-5 は Bedrock を持たない。
    #[tokio::test]
    async fn binding_does_not_leak_across_models() {
        let r = router();
        let s = session("s1");

        let fable = r.routes_for("claude-fable-5", &s).await.unwrap();
        r.remember(&s, "claude-fable-5", &fable[0]).await;

        let opus = r.routes_for("claude-opus-5", &s).await.unwrap();
        assert_eq!(names(&opus), vec!["oauth-a", "oauth-b"]);
    }

    /// 古い結びつきは捨てる。常駐なので、溜め続けると増える一方になる。
    #[tokio::test]
    async fn stale_bindings_are_evicted() {
        let r = router();
        let s = session("s1");
        let routes = r.routes_for("claude-opus-5", &s).await.unwrap();
        r.remember(&s, "claude-opus-5", &routes[1]).await;
        assert_eq!(r.affinity_len().await, 1);

        // 最後に使われた時刻を TTL より前に戻す。
        {
            let mut affinity = r.affinity.lock().await;
            let b = affinity.get_mut(&s).unwrap();
            b.seen = Instant::now() - AFFINITY_TTL - Duration::from_secs(1);
        }

        let after = r.routes_for("claude-opus-5", &s).await.unwrap();
        assert_eq!(names(&after), vec!["oauth-a", "oauth-b"], "設定の順に戻る");
        assert_eq!(r.affinity_len().await, 0, "掃除される");
    }

    /// 公開モデルの一覧はモデルピッカーに出す。
    #[tokio::test]
    async fn lists_configured_models() {
        let r = router();
        let mut models: Vec<&str> = r.models().collect();
        models.sort_unstable();
        assert_eq!(
            models,
            vec!["claude-fable-5", "claude-opus-5", "gpt-5.6-sol"]
        );
    }

    /// upstream_name は Bedrock 経路にだけ効く。公式に Bedrock の名前を
    /// 送ると 404 になるので、経路ごとに持たせる。
    #[tokio::test]
    async fn upstream_name_applies_only_to_bedrock() {
        use crate::backend::anthropic::Headers;
        use serde_json::json;

        let r = router();
        let routes = r
            .routes_for("claude-fable-5", &session("s1"))
            .await
            .unwrap();

        let mut body = json!({"model": "claude-fable-5"});
        routes[0].provider.adapt(&mut body, &mut Headers::default());
        assert_eq!(
            body["model"], "anthropic.claude-fable-5",
            "bedrock は替える"
        );

        let mut body = json!({"model": "claude-fable-5"});
        routes[1].provider.adapt(&mut body, &mut Headers::default());
        assert_eq!(body["model"], "claude-fable-5", "公式はそのまま");
    }
}
