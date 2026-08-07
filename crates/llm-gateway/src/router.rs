//! どのモデルをどの経路へ送るかを決める。
//!
//! 公開するモデルは upstream に聞いて集める。手で並べると新しいモデルが出る
//! たびに追記が要るし、書き忘れたものは 404 になる。
//!
//! 同じ会話は同じ経路に貼り続ける。貼り直すと prompt cache が無駄になり、
//! upstream から見てアカウントをまたいだ利用にも見える。
//!
//! 経路が今使えるかは**経路自身に聞く** (DR-0014 §3)。router は経路の名前を
//! 鍵にした締め出しの表を持たない — 断られ方の意味を知っているのは provider の
//! 側で、こちらが要るのは「使えるか」と「駄目なら次はいつか」だけ。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::config::{Config, Namespace, RouteSpec};
use crate::credential::{CredentialId, CredentialStore, Persistence};
use crate::denial::{Availability, PROBE_INTERVAL};
use crate::discovery::{self, Model};
use crate::egress::{Headers, Response};
use crate::events::{self, Events};
use crate::preset;
use crate::provider::Preset;
use crate::session::SessionKey;
use crate::{Error, Result};

/// 会話と経路の結びつきを保つ時間。
///
/// 短いと同じ会話が別の経路へ飛んで prompt cache を捨てることになり、
/// 長いと落ちた経路に貼り付いたままになる。
const AFFINITY_TTL: Duration = Duration::from_secs(3600);

/// 1 経路。どの upstream へ、どの認証情報で送るか。
pub struct Route {
    /// この経路の provider 実装一式と、この経路の状態。
    pub preset: Arc<Preset>,
    /// 転送先が認証を持つ経路 (relay) では要らない。
    pub credential: Option<CredentialId>,
    /// upstream での名前がクライアントの名前と違う場合だけ入る。
    ///
    /// 何という名前で受け付けるかは discovery が答える (upstream によっては
    /// 独自の名前空間が付く)。方言の話ではないので Wire は持たない。
    pub upstream_model: Option<String>,
}

impl Route {
    /// ログや失敗記録に出す名前。
    pub fn name(&self) -> &str {
        self.preset.name()
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("preset", &self.preset.name())
            .field("credential", &self.credential)
            .field("upstream_model", &self.upstream_model)
            .finish()
    }
}

/// 今このリクエストで試せる経路。
pub enum Selection {
    /// 断られていない経路。設定の優先順のまま。
    Ready(Vec<Arc<Route>>),
    /// どれも断られている。組み立て済みの応答と、最初に開く時刻。
    AllDenied {
        response: Response,
        /// 最も早く開く経路の時刻 (Unix 秒)。
        until: i64,
    },
}

impl std::fmt::Debug for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(routes) => f.debug_tuple("Ready").field(routes).finish(),
            Self::AllDenied { until, .. } => {
                f.debug_struct("AllDenied").field("until", until).finish()
            }
        }
    }
}

/// upstream に聞いて分かったこと。
///
/// **namespace で絞る前の生の一覧**。問い合わせは credential 単位で 1 回だけ
/// 行い、誰に見せるかは参照時に決める。namespace ごとに聞きに行っても、
/// 同じアカウントからは同じ答えしか返らない。
#[derive(Default)]
struct Catalog {
    /// credential 名 → その credential が扱えるモデル
    /// (クライアント向けの名前 → upstream での名前)。
    ///
    /// 独自の名前空間を付ける upstream があるので、変換が要る。
    by_route: BTreeMap<String, BTreeMap<String, String>>,
}

impl Catalog {
    /// この namespace に見せるモデルと、それを扱える credential。
    fn visible(&self, ns: &Namespace, config: &Config) -> BTreeMap<String, Vec<String>> {
        let mut visible: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (credential, models) in &self.by_route {
            for model in models.keys() {
                if ns.allows(credential, model, config) {
                    visible
                        .entry(model.clone())
                        .or_default()
                        .push(credential.clone());
                }
            }
        }
        visible
    }

    fn upstream_name(&self, credential: &str, model: &str) -> Option<&str> {
        self.by_route
            .get(credential)?
            .get(model)
            .map(String::as_str)
    }
}

pub struct Router {
    config: Config,
    /// 設定 1 件につき 1 つ。**経路の状態を持つので作り直さない** —
    /// リクエストごとに組み直すと、断られた印も枠の観測も毎回消える。
    presets: BTreeMap<String, Arc<Preset>>,
    catalog: RwLock<Catalog>,
    /// 会話と経路の結びつき。鍵は (namespace 名, 会話)。
    ///
    /// namespace が違えば見えるモデルも経路の順も違うので、会話だけで引くと
    /// 別の namespace の結果が混ざる。同じ本文から derive した会話の鍵は
    /// namespace をまたいでも一致するので、分けないと経路の順が汚れる。
    ///
    /// **provider 間で選ぶための状態**なので、経路の側ではなく core が持つ
    /// (DR-0014 §3 の横断機構)。
    affinity: Mutex<HashMap<(String, SessionKey), Binding>>,
    /// 起きたことを見ている人へ流す口。全 provider ぶんで 1 本。
    events: Arc<Events>,
}

struct Binding {
    route: Arc<Route>,
    /// 同じモデルの会話にだけ効かせる。モデルが変われば選び直す。
    model: String,
    seen: Instant,
}

impl Router {
    pub fn new(config: Config, events: Arc<Events>) -> Self {
        let presets = config
            .routes
            .iter()
            .map(|(name, route)| {
                (
                    name.clone(),
                    Arc::new(preset::from_spec(name, route, &config)),
                )
            })
            .collect();
        Self {
            config,
            presets,
            catalog: RwLock::new(Catalog::default()),
            affinity: Mutex::new(HashMap::new()),
            events,
        }
    }

    /// 起きたことを流す口。
    pub fn events(&self) -> &Arc<Events> {
        &self.events
    }

    /// 名前で経路の preset を引く。設定に無ければ `None`。
    pub fn preset(&self, name: &str) -> Option<&Arc<Preset>> {
        self.presets.get(name)
    }

    /// 設定順の (名前, preset)。
    pub fn presets(&self) -> impl Iterator<Item = (&str, &Arc<Preset>)> {
        self.presets
            .iter()
            .map(|(name, preset)| (name.as_str(), preset))
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
        let mut by_route: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let previous = self.catalog.read().await;

        for (name, route) in &self.config.routes {
            let found = match route.discovery_flavor(&self.config) {
                Some(flavor) => match self.discover(http, credentials, name, route, flavor).await {
                    Ok(found) => found,
                    Err(e) => {
                        warn!(credential = %name, %e, "一覧を取れません。前回の結果を使います");
                        previous.by_route.get(name).cloned().unwrap_or_default()
                    }
                },
                // 聞けない upstream は設定に書かれたものを使う。
                None => route
                    .declared_models()
                    .iter()
                    .map(|m| (m.clone(), m.clone()))
                    .collect(),
            };
            by_route.insert(name.clone(), found);
        }
        drop(previous);

        let total: usize = by_route.values().map(BTreeMap::len).sum();
        info!(
            credentials = by_route.len(),
            models = total,
            "モデル一覧を更新しました"
        );
        *self.catalog.write().await = Catalog { by_route };
    }

    /// 1 つの credential に一覧を聞く。
    ///
    /// 返すのは「クライアント向けの名前 → upstream での名前」。
    async fn discover<P: Persistence>(
        &self,
        http: &reqwest::Client,
        credentials: &CredentialStore<P>,
        name: &str,
        route: &RouteSpec,
        flavor: discovery::Flavor,
    ) -> Result<BTreeMap<String, String>> {
        let credential_name = route.credential.as_deref().ok_or_else(|| {
            Error::Config(format!(
                "route `{name}` には discovery 用 credential がありません"
            ))
        })?;
        let credential = credentials
            .acquire(&CredentialId::new(credential_name))
            .await?;
        let found = discovery::fetch(http, flavor, route.url(), &credential).await?;
        Ok(found.into_iter().map(|m| (m.id, m.upstream_id)).collect())
    }

    /// この namespace に見せるモデル名 (エイリアスを含む)。
    ///
    /// 一覧そのものは共有だが、**何を見せるかは namespace ごとに違う**。
    pub async fn models(&self, ns: &Namespace) -> Vec<String> {
        let catalog = self.catalog.read().await;
        let visible = catalog.visible(ns, &self.config);

        let mut all: Vec<String> = visible.keys().cloned().collect();
        all.extend(aliases_for(ns, &visible).into_keys());
        all.sort_unstable();
        all.dedup();
        all
    }

    /// エイリアスなら実際のモデル名に直す。
    pub async fn resolve(&self, ns: &Namespace, model: &str) -> String {
        let catalog = self.catalog.read().await;
        let visible = catalog.visible(ns, &self.config);
        aliases_for(ns, &visible)
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned())
    }

    /// このモデルを実際に試す順。表示用。
    ///
    /// 設定の優先順そのままではなく、**そのモデルを扱える credential だけ**に
    /// 絞ったもの。設定を見ただけでは分からないので、確認に使える。
    pub async fn route_names(&self, ns: &Namespace, model: &str) -> Vec<String> {
        let catalog = self.catalog.read().await;
        let visible = catalog.visible(ns, &self.config);
        let model = aliases_for(ns, &visible)
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned());

        let Some(available) = visible.get(&model) else {
            return Vec::new();
        };
        ns.routes_for(&model, &self.config)
            .into_iter()
            .filter(|name| available.iter().any(|a| a == name))
            .map(str::to_owned)
            .collect()
    }

    /// この会話でこのモデルを使うときの経路を、試す順に返す。
    ///
    /// `ns_name` は前回通った経路を引くための鍵。`ns` から名前は取れないので
    /// 呼び出し側から渡す。
    pub async fn routes_for(
        &self,
        ns: &Namespace,
        ns_name: &str,
        model: &str,
        session: &SessionKey,
    ) -> Result<Vec<Arc<Route>>> {
        let catalog = self.catalog.read().await;
        let visible = catalog.visible(ns, &self.config);
        let available = visible
            .get(model)
            .ok_or_else(|| Error::UnknownModel(model.to_owned()))?;

        // 設定の優先順のうち、このモデルを実際に扱えるものだけを残す。
        let ordered: Vec<&str> = ns
            .routes_for(model, &self.config)
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

        let key = (ns_name.to_owned(), session.clone());
        if let Some(bound) = affinity.get(&key).filter(|b| b.model == model)
            && let Some(at) = routes.iter().position(|r| r.name() == bound.route.name())
        {
            // 抜いて先頭へ差し込む。入れ替えると、先頭にいた経路が抜けた穴へ
            // 飛んで残りの優先順が入れ替わる (前回通った経路の後ろは、設定に
            // 書いた順のままであってほしい)。
            let bound = routes.remove(at);
            routes.insert(0, bound);
        }
        Ok(routes)
    }

    /// 今このリクエストで試せる経路を選ぶ。
    ///
    /// 断られている経路は**外す**。開く時刻を知っていながら実リクエストを
    /// 当てるのは、分かっている壁にわざわざぶつかりに行くのと同じ。
    ///
    /// 全滅なら 429 をここで組む。「候補が空」は経路を選ぶ側の判断なので、
    /// その答えも選ぶ側が出す (DR-0014 §8)。見ている人にも 1 件流す — upstream
    /// を叩いていないだけで、クライアントには断りが返っている。`origin` の
    /// `credential` は呼び出し側が決める (この応答を出したのはどの経路でもない)。
    pub fn select(
        &self,
        routes: &[Arc<Route>],
        model: &str,
        now: i64,
        origin: &events::Origin<'_>,
    ) -> Selection {
        let mut ready = Vec::new();
        let mut opens_at = Vec::new();
        for route in routes {
            match route.preset.availability(model, now) {
                Availability::Ready => ready.push(Arc::clone(route)),
                Availability::Denied { until } => opens_at.push(until),
            }
        }
        if !ready.is_empty() {
            return Selection::Ready(ready);
        }

        // どれも塞がっているなら、最初に開くのがいつかがクライアントの知りたいこと。
        let until = opens_at.into_iter().min().unwrap_or(now);
        warn!(
            model = %model,
            routes = routes.len(),
            seconds = until - now,
            "どの経路も断られています。開く時刻を伝えて返します"
        );
        self.events.publish(events::Event::new(now, origin, 429));
        Selection::AllDenied {
            response: rate_limited(until - now),
            until,
        }
    }

    fn build_route(&self, catalog: &Catalog, name: &str, model: &str) -> Option<Arc<Route>> {
        let route = self.config.routes.get(name)?;
        let preset = self.presets.get(name)?;
        Some(Arc::new(Route {
            preset: Arc::clone(preset),
            credential: route.credential.as_deref().map(CredentialId::new),
            // upstream での名前が違う場合だけ書き換える。
            upstream_model: catalog
                .upstream_name(name, model)
                .filter(|upstream| *upstream != model)
                .map(str::to_owned),
        }))
    }

    /// 実際に使えた経路を覚える。
    pub async fn remember(
        &self,
        ns_name: &str,
        session: &SessionKey,
        model: &str,
        route: &Arc<Route>,
    ) {
        self.affinity.lock().await.insert(
            (ns_name.to_owned(), session.clone()),
            Binding {
                route: Arc::clone(route),
                model: model.to_owned(),
                seen: Instant::now(),
            },
        );
    }

    /// discovery が済んだ状態を作る。
    ///
    /// 引数は「credential → その credential が扱えるモデル
    /// (クライアント向けの名前, upstream での名前)」。
    #[cfg(test)]
    async fn set_catalog(&self, by_route: &[(&str, &[(&str, &str)])]) {
        self.catalog.write().await.by_route = by_route
            .iter()
            .map(|(cred, models)| {
                (
                    (*cred).to_owned(),
                    models
                        .iter()
                        .map(|(id, upstream)| ((*id).to_owned(), (*upstream).to_owned()))
                        .collect(),
                )
            })
            .collect();
    }
}

/// どの経路も断られているときに返す応答。
///
/// 開く時刻を知っているのだから、実リクエストを当てて 429 を貰い直す必要は
/// ない。クライアントが次の一手を決めるのに要るのは状態コードと
/// `retry-after` で、それはこちらで組み立てられる (DR-0009)。
///
/// 待たせる長さは [`PROBE_INTERVAL`] で頭を押さえる。裏で聞きに行った結果、
/// 宣言されたリセット時刻より早く開くことがある。2 日後と伝えてしまうと、
/// 早期に開いたことに気づいた側から見て嘘になる。
fn rate_limited(after: i64) -> Response {
    let after = after.min(PROBE_INTERVAL);
    const BODY: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"every route for this model is rate limited or overloaded; see the retry-after header"}}"#;
    Response {
        status: 429,
        headers: Headers::new(vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("retry-after".to_owned(), after.max(1).to_string()),
        ]),
        body: Box::pin(futures_util::stream::once(std::future::ready(Ok(
            bytes::Bytes::from_static(BODY.as_bytes()),
        )))),
    }
}

/// この namespace で使えるエイリアス。
///
/// 見えているモデルの中から解決する。同じ設定でも、namespace が違えば
/// 見えるモデルが違うので、行き先も変わる。
fn aliases_for(
    ns: &Namespace,
    visible: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, String> {
    let known: Vec<Model> = visible
        .keys()
        .map(|id| Model {
            id: id.clone(),
            upstream_id: id.clone(),
            created: 0,
        })
        .collect();
    resolve_aliases(&ns.resolved_aliases(), &known)
}

/// エイリアスを実際のモデル名まで解決する。
///
/// 値には実モデルのパターン (`model-pro-*`) だけでなく、**別のエイリアス**も
/// 書ける。`pro = "model-pro"` / `model-pro = "model-pro-*"` のように段を
/// 分けると、正式な短縮名と手癖の短縮名を別々に管理できる。
///
/// 循環 (`a = "b"`, `b = "a"`) を書いてしまった分は捨てる。名前解決が
/// 戻ってこないより、そのエイリアスが無い方がまだ分かりやすい。
fn resolve_aliases(
    aliases: &BTreeMap<String, String>,
    known: &[Model],
) -> BTreeMap<String, String> {
    let mut resolved = BTreeMap::new();

    for (name, value) in aliases {
        let mut seen = vec![name.clone()];
        let mut cursor = value.clone();

        let target = loop {
            // 実モデルに当たるならそれで確定。
            if let Some(hit) = discovery::resolve_alias(&cursor, known) {
                break Some(hit);
            }
            // 当たらないなら別のエイリアスを指しているとみなして辿る。
            let Some(next) = aliases.get(&cursor) else {
                break None;
            };
            if seen.contains(&cursor) {
                warn!(alias = %name, "エイリアスが循環しています。この定義は使いません");
                break None;
            }
            seen.push(cursor.clone());
            cursor = next.clone();
        };

        if let Some(target) = target {
            resolved.insert(name.clone(), target);
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    const CONFIG: &str = r#"
[ns.default.filter]
exclude = ["claude-opus-4*"]

[credentials.bedrock]
type = "bedrock_api_key"

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock.invalid/anthropic"

[credentials.oauth-a]
type = "claude_oauth"

[routes.oauth-a]
provider = "anthropic"
credential = "oauth-a"

[credentials.oauth-b]
type = "claude_oauth"

[routes.oauth-b]
provider = "anthropic"
credential = "oauth-b"
exclude = ["claude-haiku-*"]

[routes.cpa]
provider = "anthropic"
url = "http://127.0.0.1:8320"
models = ["gpt-5.6-sol"]

[[ns.default.routing]]
models = ["claude-fable-*"]
routes = ["bedrock", "oauth-a"]

# 3 本並ぶ経路。前回通った経路を先頭へ寄せたときに、残りの順が
# 設定のままかを見るために使う。
[[ns.default.routing]]
models = ["claude-sonnet-5"]
routes = ["bedrock", "oauth-a", "oauth-b"]

[[ns.default.routing]]
models = ["gpt-*"]
routes = ["cpa"]

[ns.default.aliases]
# 正式な短縮名
claude-fable = "claude-fable-*"
claude-opus = "claude-opus-*"
claude-haiku = "claude-haiku-*"
# さらに短い名前 (上の短縮名を指す)
fable = "claude-fable"
opus = "claude-opus"
haiku = "claude-haiku"

# 結びつきが namespace をまたがないことを見るための面。
[[ns.other.routing]]
models = ["claude-fable-*"]
routes = ["bedrock", "oauth-a"]
"#;

    const NOW: i64 = 1_800_000_000;

    fn build(config: Config) -> Router {
        Router::new(config, Arc::new(Events::new()))
    }

    /// discovery 済みの状態を作る。
    async fn router() -> Router {
        let config: Config = toml::from_str(CONFIG).unwrap();
        config.validate().unwrap();
        let r = build(config);
        r.set_catalog(&[
            // Bedrock は fable / sonnet を扱い、upstream では名前空間が付く。
            (
                "bedrock",
                &[
                    ("claude-fable-5", "anthropic.claude-fable-5"),
                    ("claude-sonnet-5", "anthropic.claude-sonnet-5"),
                ],
            ),
            (
                "oauth-a",
                &[
                    ("claude-fable-5", "claude-fable-5"),
                    ("claude-opus-5", "claude-opus-5"),
                    ("claude-sonnet-5", "claude-sonnet-5"),
                    ("claude-haiku-4-5-20251001", "claude-haiku-4-5-20251001"),
                ],
            ),
            (
                "oauth-b",
                &[
                    ("claude-opus-5", "claude-opus-5"),
                    ("claude-sonnet-5", "claude-sonnet-5"),
                    ("claude-haiku-4-5-20251001", "claude-haiku-4-5-20251001"),
                ],
            ),
            ("cpa", &[("gpt-5.6-sol", "gpt-5.6-sol")]),
        ])
        .await;
        r
    }

    /// 試験で使う namespace 名。
    const NS: &str = crate::config::DEFAULT_NAMESPACE;

    /// 既定の namespace。
    fn ns(r: &Router) -> &Namespace {
        r.config.namespace(NS).expect("既定は必ずある")
    }

    fn session(name: &str) -> SessionKey {
        crate::session::derive(&serde_json::json!({"conversation_id": name}), &[])
    }

    fn names(routes: &[Arc<Route>]) -> Vec<&str> {
        routes.iter().map(|r| r.name()).collect()
    }

    fn origin(model: &str) -> events::Origin<'_> {
        events::Origin {
            session_id: None,
            prefix: None,
            ns: NS,
            model,
            credential: crate::stats::NO_CREDENTIAL,
        }
    }

    #[tokio::test]
    async fn follows_routing_rules() {
        let r = router().await;
        let got = r
            .routes_for(ns(&r), NS, "claude-fable-5", &session("s1"))
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a"]);
    }

    /// 規則に無いモデルは、扱える credential を宣言順に試す。
    /// 新しいモデルが出ても設定を触らずに使える。
    #[tokio::test]
    async fn unrouted_model_uses_whoever_can_serve_it() {
        let r = router().await;
        let got = r
            .routes_for(ns(&r), NS, "claude-opus-5", &session("s1"))
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["oauth-a", "oauth-b"]);
    }

    /// そのモデルを扱えない credential は候補に入らない。
    #[tokio::test]
    async fn excluded_credential_is_not_offered() {
        let r = router().await;
        let got = r
            .routes_for(ns(&r), NS, "claude-haiku-4-5-20251001", &session("s1"))
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
            .routes_for(ns(&r), NS, "no-such-model", &session("s1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no-such-model"), "{err}");
    }

    /// upstream で名前が違うものだけ書き換える。
    ///
    /// 何という名前で受け付けるかは discovery が答えるので、経路に添えて
    /// 持ち回る (方言の実装は関知しない)。
    #[tokio::test]
    async fn carries_the_upstream_model_name_only_where_needed() {
        let r = router().await;
        let routes = r
            .routes_for(ns(&r), NS, "claude-fable-5", &session("s1"))
            .await
            .unwrap();

        assert_eq!(
            routes[0].upstream_model.as_deref(),
            Some("anthropic.claude-fable-5"),
            "Bedrock は名前空間つき"
        );
        assert_eq!(routes[1].upstream_model, None, "公式はそのまま");
    }

    /// 経路は設定 1 件につき 1 つの preset を共有する。
    ///
    /// リクエストごとに組み直すと、その経路が覚えた締め出しも枠も毎回消える。
    #[tokio::test]
    async fn routes_share_one_persistent_preset_per_credential() {
        let r = router().await;
        let s = session("s1");

        let first = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();
        let again = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();

        assert!(
            Arc::ptr_eq(&first[0].preset, &again[0].preset),
            "同じ実体を配る"
        );
        // 別のモデルで引いた経路も、同じ credential なら同じ preset。
        let other_model = r
            .routes_for(ns(&r), NS, "claude-sonnet-5", &s)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first[0].preset, &other_model[0].preset));
    }

    /// 締め出しの印は経路が持つので、次の routes_for でも生きている。
    #[tokio::test]
    async fn a_denial_survives_the_next_lookup() {
        let r = router().await;
        let s = session("s1");
        let routes = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();
        routes[0]
            .preset
            .reject(429, &Headers::default(), "claude-fable-5", NOW)
            .expect("429 は締め出す");

        let again = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();
        assert!(matches!(
            again[0].preset.availability("claude-fable-5", NOW),
            Availability::Denied { .. }
        ));
    }

    /// 断られている経路は候補から外す。
    #[tokio::test]
    async fn select_drops_the_denied_routes() {
        let r = router().await;
        let routes = r
            .routes_for(ns(&r), NS, "claude-fable-5", &session("s1"))
            .await
            .unwrap();
        routes[0]
            .preset
            .reject(429, &Headers::default(), "claude-fable-5", NOW);

        let Selection::Ready(ready) =
            r.select(&routes, "claude-fable-5", NOW, &origin("claude-fable-5"))
        else {
            panic!("1 本は残る");
        };
        assert_eq!(names(&ready), vec!["oauth-a"]);
    }

    /// 全滅なら router が 429 を組む。開く時刻は最も早いものを伝える。
    #[tokio::test]
    async fn every_route_denied_is_answered_by_the_router() {
        let r = router().await;
        let model = "claude-fable-5";
        let routes = r
            .routes_for(ns(&r), NS, model, &session("s1"))
            .await
            .unwrap();
        for route in &routes {
            route
                .preset
                .reject(429, &Headers::default(), model, NOW)
                .expect("429 は締め出す");
        }

        let Selection::AllDenied { response, until } =
            r.select(&routes, model, NOW, &origin(model))
        else {
            panic!("全滅のはず");
        };

        assert_eq!(response.status, 429);
        assert_eq!(until, NOW + 60, "最初に開く時刻");
        assert_eq!(response.headers.get("retry-after"), Some("60"));
        assert_eq!(
            response.headers.get("content-type"),
            Some("application/json")
        );

        let body = response
            .body
            .fold(Vec::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(&chunk.unwrap());
                acc
            })
            .await;
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("rate_limit_error"),
            "クライアントが読む形で返す"
        );
    }

    /// 自前で返した 429 も、見ている人には 1 件流れる。
    ///
    /// upstream を叩いていないだけで、クライアントには断りが返っている。
    /// 流さないと、webhook や SSE から「返事が消えた」ように見える。
    #[tokio::test]
    async fn the_self_made_denial_is_announced_too() {
        let r = router().await;
        let model = "claude-fable-5";
        let routes = r
            .routes_for(ns(&r), NS, model, &session("s1"))
            .await
            .unwrap();
        for route in &routes {
            route.preset.reject(429, &Headers::default(), model, NOW);
        }

        let mut watching = r.events().subscribe();
        r.select(&routes, model, NOW, &origin(model));

        let event = watching.recv().await.unwrap();
        assert_eq!(event.status, 429);
        assert_eq!(event.model, model);
        assert_eq!(event.ns, NS);
        assert_eq!(
            event.credential,
            crate::stats::NO_CREDENTIAL,
            "答えたのは gateway 自身で、どの credential でもない"
        );
    }

    /// 待たせる長さは、様子を聞きに行く間隔で頭を押さえる。
    ///
    /// 宣言されたリセット時刻より早く開くことがあり、その早期回復に気づく
    /// のは裏で聞きに行った時。2 日後と伝えると、気づいた側から見て嘘になる。
    #[test]
    fn the_retry_after_is_capped_at_the_probe_interval() {
        let resp = rate_limited(2 * 24 * 3600);
        assert_eq!(
            resp.headers.get("retry-after"),
            Some(PROBE_INTERVAL.to_string().as_str())
        );
    }

    /// 開く時刻が過ぎていても、0 秒とは伝えない。
    #[test]
    fn the_retry_after_is_never_zero() {
        assert_eq!(rate_limited(0).headers.get("retry-after"), Some("1"));
        assert_eq!(rate_limited(-10).headers.get("retry-after"), Some("1"));
    }

    /// エイリアスは一番新しいものに向く。
    #[tokio::test]
    async fn aliases_resolve_to_concrete_models() {
        let r = router().await;
        assert_eq!(r.resolve(ns(&r), "opus").await, "claude-opus-5");
        assert_eq!(r.resolve(ns(&r), "fable").await, "claude-fable-5");
        assert_eq!(
            r.resolve(ns(&r), "haiku").await,
            "claude-haiku-4-5-20251001"
        );
    }

    /// エイリアスでない名前はそのまま通す。
    #[tokio::test]
    async fn non_alias_passes_through() {
        let r = router().await;
        assert_eq!(r.resolve(ns(&r), "claude-opus-5").await, "claude-opus-5");
        assert_eq!(r.resolve(ns(&r), "unknown").await, "unknown");
    }

    /// 一覧にはエイリアスも並べる。短い名前で選べるようにする。
    #[tokio::test]
    async fn model_list_includes_aliases() {
        let r = router().await;
        let models = r.models(ns(&r)).await;
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

        let first = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();
        r.remember(NS, &s, "claude-fable-5", &first[1]).await;

        let again = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();
        assert_eq!(names(&again), vec!["oauth-a", "bedrock"]);
        assert_eq!(again.len(), 2, "候補は減らさない");
    }

    /// 前回通った経路を先頭へ寄せても、残りは設定の優先順のまま。
    ///
    /// 入れ替えで寄せると、先頭にいた経路が抜けた穴へ飛んで順が崩れる。
    /// 崩れた順は、断られた経路を外した後の「次に試す先」を変えてしまう。
    #[tokio::test]
    async fn moving_the_remembered_route_keeps_the_rest_in_order() {
        let r = router().await;
        let s = session("s1");

        let routes = r
            .routes_for(ns(&r), NS, "claude-sonnet-5", &s)
            .await
            .unwrap();
        assert_eq!(names(&routes), vec!["bedrock", "oauth-a", "oauth-b"]);

        r.remember(NS, &s, "claude-sonnet-5", &routes[2]).await;
        assert_eq!(
            names(
                &r.routes_for(ns(&r), NS, "claude-sonnet-5", &s)
                    .await
                    .unwrap()
            ),
            vec!["oauth-b", "bedrock", "oauth-a"],
            "寄せた 1 本以外は動かさない"
        );
    }

    #[tokio::test]
    async fn bindings_are_per_session_and_model() {
        let r = router().await;
        let s = session("s1");

        let routes = r
            .routes_for(ns(&r), NS, "claude-fable-5", &s)
            .await
            .unwrap();
        r.remember(NS, &s, "claude-fable-5", &routes[1]).await;

        assert_eq!(
            names(
                &r.routes_for(ns(&r), NS, "claude-fable-5", &session("s2"))
                    .await
                    .unwrap()
            ),
            vec!["bedrock", "oauth-a"],
            "別の会話には効かない"
        );
        assert_eq!(
            names(&r.routes_for(ns(&r), NS, "claude-opus-5", &s).await.unwrap()),
            vec!["oauth-a", "oauth-b"],
            "別のモデルには効かない"
        );
    }

    /// 別の namespace で覚えた結びつきは持ち込まない。
    ///
    /// 会話の鍵は本文から derive するので、namespace が違っても一致しうる。
    /// 混ざると、片方の面で断られた経路がもう片方の先頭に来る。
    #[tokio::test]
    async fn bindings_do_not_cross_namespaces() {
        let r = router().await;
        let s = session("s1");
        let other = r.config.namespace("other").expect("設定にある");

        let routes = r
            .routes_for(other, "other", "claude-fable-5", &s)
            .await
            .unwrap();
        r.remember("other", &s, "claude-fable-5", &routes[1]).await;

        assert_eq!(
            names(
                &r.routes_for(other, "other", "claude-fable-5", &s)
                    .await
                    .unwrap()
            ),
            vec!["oauth-a", "bedrock"],
            "覚えた面では効く"
        );
        assert_eq!(
            names(
                &r.routes_for(ns(&r), NS, "claude-fable-5", &s)
                    .await
                    .unwrap()
            ),
            vec!["bedrock", "oauth-a"],
            "同じ会話の鍵でも、別の面の順は変わらない"
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
                .routes_for(ns(&r), NS, model, &s)
                .await
                .unwrap()
                .iter()
                .map(|route| route.name().to_owned())
                .collect();
            assert_eq!(r.route_names(ns(&r), model).await, actual, "{model}");
        }
    }

    /// エイリアスがエイリアスを指してもよい。
    ///
    /// 長い短縮名と短い短縮名を分けて書けるようにするため。
    #[tokio::test]
    async fn aliases_can_point_at_other_aliases() {
        let r = router().await;
        assert_eq!(
            r.resolve(ns(&r), "claude-opus").await,
            "claude-opus-5",
            "一段目"
        );
        assert_eq!(r.resolve(ns(&r), "opus").await, "claude-opus-5", "二段目");
        assert_eq!(r.resolve(ns(&r), "fable").await, "claude-fable-5");
        assert_eq!(
            r.resolve(ns(&r), "haiku").await,
            "claude-haiku-4-5-20251001"
        );
    }

    /// 循環したエイリアスは捨てる。名前解決が戻ってこないよりまし。
    #[test]
    fn circular_aliases_are_dropped() {
        let known = vec![Model {
            id: "claude-opus-5".into(),
            upstream_id: "claude-opus-5".into(),
            created: 0,
        }];
        let aliases = BTreeMap::from([
            ("a".to_owned(), "b".to_owned()),
            ("b".to_owned(), "a".to_owned()),
            ("ok".to_owned(), "claude-opus-*".to_owned()),
        ]);

        let resolved = resolve_aliases(&aliases, &known);
        assert!(!resolved.contains_key("a"), "循環は落とす");
        assert!(!resolved.contains_key("b"));
        assert_eq!(resolved["ok"], "claude-opus-5", "他は巻き添えにしない");
    }

    /// どこにも当たらないエイリアスは捨てる (一覧に出さない)。
    #[test]
    fn unresolvable_aliases_are_dropped() {
        let known = vec![Model {
            id: "claude-opus-5".into(),
            upstream_id: "claude-opus-5".into(),
            created: 0,
        }];
        let aliases = BTreeMap::from([("gone".to_owned(), "claude-gemini-*".to_owned())]);
        assert!(resolve_aliases(&aliases, &known).is_empty());
    }

    /// エイリアスでも実際の経路を出す。
    #[tokio::test]
    async fn route_names_resolves_aliases() {
        let r = router().await;
        assert_eq!(
            r.route_names(ns(&r), "fable").await,
            r.route_names(ns(&r), "claude-fable-5").await
        );
    }

    #[tokio::test]
    async fn route_names_is_empty_for_unknown_model() {
        let r = router().await;
        assert!(r.route_names(ns(&r), "no-such-model").await.is_empty());
    }

    /// 一覧が空なら何も出さない (起動直後で discovery 前の状態)。
    #[tokio::test]
    async fn empty_catalog_serves_nothing() {
        let config: Config = toml::from_str(CONFIG).unwrap();
        let r = build(config);
        assert!(r.models(ns(&r)).await.is_empty());
        assert!(
            r.routes_for(ns(&r), NS, "claude-opus-5", &session("s"))
                .await
                .is_err()
        );
    }
}
