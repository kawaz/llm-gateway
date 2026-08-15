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

use crate::config::{self, Config, Namespace, RouteSpec};
use crate::credential::time::now_unix;
use crate::credential::{CredentialId, CredentialStore, Persistence};
use crate::denial::{Availability, Denial, PROBE_INTERVAL, Reason, Scope};
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
    /// 借りてよいのは経過した時間ぶんまで、という上限 (DR-0019)。
    /// 書いていなければ無い。
    ///
    /// **経路 (preset) ではなくここが持つ**。同じ credential でも namespace や
    /// モデル群が変われば上限は変わるので、経路の状態に混ぜると面をまたいで
    /// 効いてしまう。ここは 1 リクエストぶんの組み立て結果なので、書いた
    /// 規則のとおりにだけ効く。
    pub pace_cap: Option<config::PaceCap>,
}

impl Route {
    /// ログや失敗記録に出す名前。
    pub fn name(&self) -> &str {
        self.preset.name()
    }

    /// 按分線を超えて使ったか。超えていれば、次に予算が増えるまでの控え。
    ///
    /// 見るのは**周期が一番長い窓**だけ (DR-0018 §2 と同じ理由 — 数時間で
    /// 回る窓に借用の話は無い)。窓長・リセット時刻・使用率のどれかが読めない
    /// 場合は**通さない**: 上限を書いた経路は「どれだけ使ったか分かっている
    /// こと」が前提で、分からないまま通すと上限を書いていないのと同じになる
    /// (DR-0019 §5)。
    ///
    /// 返すのは断りの形 ([`Denial`]) だが、**経路には控えない**。上限は
    /// namespace × モデル群ごとに違うので、経路の状態に置くと面をまたいで
    /// 効いてしまう (借りる側で締めた上限が、貸す側の面まで止める)。
    pub fn paced_out(&self, now: i64) -> Option<Denial> {
        let cap = self.pace_cap?;
        let hold = |until| {
            Some(Denial {
                until,
                reason: Reason::Paced,
                scope: Scope::Everything,
            })
        };
        let Some(state) = self.pace_state(now) else {
            // 読めないので、次にいつ開くとも言えない。短く閉じて、その間に
            // 枠を聞き直す猶予を作る。
            return hold(now + crate::denial::DEFAULT_BACKOFF);
        };
        let budget = cap.budget(state.window_seconds, state.elapsed);
        if state.utilization <= budget.allowed {
            return None;
        }
        hold(state.window_started_at + budget.next_step_at)
    }

    /// 上限を判定したいのに枠が読めていない経路か (DR-0019 §5)。
    ///
    /// この状態の経路は通さないので、放っておくと閉じたままになる。呼び出し側
    /// が枠を聞き直す合図に使う。
    pub fn needs_quota(&self, now: i64) -> bool {
        self.pace_cap.is_some() && self.pace_state(now).is_none()
    }

    /// 上限の判定に要る、最長周期窓の今の姿。読めなければ `None`。
    fn pace_state(&self, now: i64) -> Option<PaceState> {
        let snapshot = self.preset.quota()?;
        let window = snapshot.longest_window()?;
        let window_seconds = window.window_seconds?;
        let reset = window.reset.filter(|reset| *reset > now)?;
        Some(PaceState {
            window_seconds,
            window_started_at: reset - window_seconds as i64,
            elapsed: window_seconds as i64 - (reset - now),
            utilization: window.utilization?,
        })
    }
}

/// 最長周期窓の今の姿 (上限の判定に使う分だけ)。
struct PaceState {
    window_seconds: u64,
    /// 窓の頭の時刻 (Unix 秒)。
    window_started_at: i64,
    /// 窓の頭からの経過 (秒)。
    elapsed: i64,
    /// 使用率 (0.0〜1.0)。
    utilization: f64,
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
                        warn!(route = %name, %e, "cannot fetch the model list; using the saved or configured list");
                        previous
                            .by_route
                            .get(name)
                            .filter(|models| !models.is_empty())
                            .cloned()
                            .unwrap_or_else(|| {
                                route
                                    .declared_models()
                                    .iter()
                                    .map(|model| (model.clone(), model.clone()))
                                    .collect()
                            })
                    }
                },
                // 聞けない upstream は設定に書かれたものを使う。
                None => route
                    .declared_models()
                    .iter()
                    .map(|m| (m.clone(), m.clone()))
                    .collect(),
            };
            by_route.insert(name.clone(), with_declared_fallback(found, route));
        }
        drop(previous);

        let total: usize = by_route.values().map(BTreeMap::len).sum();
        info!(
            credentials = by_route.len(),
            models = total,
            "updated model catalog"
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
        let credential_name = route
            .credential
            .as_deref()
            .ok_or_else(|| Error::Config(format!("route `{name}` has no discovery credential")))?;
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
        let now = now_unix();
        ns.route_groups_for(&model, &self.config)
            .into_iter()
            .flat_map(|group| {
                let mut equal: Vec<&str> = group
                    .into_iter()
                    .filter(|name| available.iter().any(|available| available == name))
                    .collect();
                equal.sort_by_key(|name| self.reset_order_key(name, now));
                equal
            })
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
        self.routes_for_at(ns, ns_name, model, session, now_unix())
            .await
    }

    async fn routes_for_at(
        &self,
        ns: &Namespace,
        ns_name: &str,
        model: &str,
        session: &SessionKey,
        now: i64,
    ) -> Result<Vec<Arc<Route>>> {
        let catalog = self.catalog.read().await;
        let visible = catalog.visible(ns, &self.config);
        let available = visible
            .get(model)
            .ok_or_else(|| Error::UnknownModel(model.to_owned()))?;

        // モデル非対応の経路を除いた後もグループ境界を保ち、同格の中だけを並べ替える。
        let mut routes = Vec::new();
        for group in ns.route_groups_for(model, &self.config) {
            let mut equal: Vec<Arc<Route>> = group
                .into_iter()
                .filter(|name| available.iter().any(|available| available == name))
                .filter_map(|name| self.build_route(&catalog, ns, name, model))
                .collect();
            equal.sort_by_key(|route| self.reset_order_key(route.name(), now));
            routes.extend(equal);
        }
        drop(catalog);

        if routes.is_empty() {
            return Err(Error::UnknownModel(model.to_owned()));
        }

        // リセットが迫っている経路を、グループ境界を越えて先頭へ (DR-0018)。
        if let Some(within) = ns.spend_down_for(model) {
            self.spend_down(&mut routes, within, now);
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
    /// 消費の上限 (DR-0019) に達した経路も同じように外す。upstream はまだ
    /// 通すが、通させたくないのはこちらの都合 — 断られたのと区別する必要が
    /// あるのは記録の側だけで、選ぶ側から見ればどちらも「今は使わない」。
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
            if let Some(held) = route.paced_out(now) {
                info!(
                    route = route.name(),
                    model = %model,
                    reason = ?held.reason,
                    seconds = held.until - now,
                    "route is ahead of its pace; holding it until the budget grows"
                );
                opens_at.push(held.until);
                continue;
            }
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
            "every route is denied; returning the time it reopens"
        );
        self.events.publish(events::Event::new(now, origin, 429));
        Selection::AllDenied {
            response: rate_limited(until - now),
            until,
        }
    }

    /// リセットが閾値まで迫った経路を先頭へ寄せる (DR-0018 §3)。
    ///
    /// 静的順・同格グループの並びは、繰り上がらなかった経路の間では保たれる。
    /// 繰り上げた分どうしはリセットが近い順。
    fn spend_down(&self, routes: &mut Vec<Arc<Route>>, within: config::WindowSpan, now: i64) {
        let mut promoted: Vec<(i64, Arc<Route>)> = Vec::new();
        let mut rest = Vec::with_capacity(routes.len());
        for route in routes.drain(..) {
            // 上限で外れる経路は昇格の対象にもしない (DR-0019 §8)。使えない
            // 経路を先頭へ寄せても、順序が 1 つ無駄になるだけ。
            let capped = route.paced_out(now).is_some();
            match self.spend_down_reset(route.name(), within, now) {
                Some(reset) if !capped => promoted.push((reset, route)),
                _ => rest.push(route),
            }
        }
        if promoted.is_empty() {
            *routes = rest;
            return;
        }
        promoted.sort_by_key(|(reset, _)| *reset);
        routes.extend(promoted.into_iter().map(|(_, route)| route));
        routes.extend(rest);
    }

    /// この経路が使い切りの対象なら、そのリセット時刻。
    ///
    /// 見るのは**周期が一番長い窓だけ** (DR-0018 §2)。窓長かリセット時刻が
    /// 取れない経路は対象にしない — 観測できていない相手を推測で繰り上げない。
    fn spend_down_reset(&self, route: &str, within: config::WindowSpan, now: i64) -> Option<i64> {
        let snapshot = self.presets.get(route)?.quota()?;
        let window = snapshot.longest_window()?;
        let length = window.window_seconds?;
        let reset = window.reset.filter(|reset| *reset > now)?;
        (reset - now <= within.seconds_in(length)).then_some(reset)
    }

    fn reset_order_key(&self, route: &str, now: i64) -> (bool, i64) {
        self.presets
            .get(route)
            .and_then(|preset| preset.quota())
            .and_then(|snapshot| snapshot.seven_day)
            .and_then(|window| window.reset)
            .filter(|reset| *reset > now)
            .map_or((true, 0), |reset| (false, reset))
    }

    fn build_route(
        &self,
        catalog: &Catalog,
        ns: &Namespace,
        name: &str,
        model: &str,
    ) -> Option<Arc<Route>> {
        let route = self.config.routes.get(name)?;
        let preset = self.presets.get(name)?;
        Some(Arc::new(Route {
            preset: Arc::clone(preset),
            credential: route.credential.as_deref().map(CredentialId::new),
            pace_cap: ns.pace_cap_for(model, name),
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
/// 一覧が空だった経路に、設定で宣言されたモデルを充てる。
///
/// 一覧が空のアカウントは実在する (Codex の /models が 200 で空を返す)。
/// 空のまま載せると経路ごと消えて、宣言していても一切選ばれなくなる。
fn with_declared_fallback(
    found: BTreeMap<String, String>,
    route: &crate::config::RouteSpec,
) -> BTreeMap<String, String> {
    if !found.is_empty() {
        return found;
    }
    route
        .declared_models()
        .iter()
        .map(|m| (m.clone(), m.clone()))
        .collect()
}

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
                warn!(alias = %name, "the alias is circular; ignoring this definition");
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
routes = [["bedrock", "oauth-a"]]

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

# 使い切りの繰り上げ (DR-0018) を見るための面。優先順は静的で、
# 昇格が起きたときだけ並びが変わる。
[[ns.spend.routing]]
models = ["claude-sonnet-5"]
routes = ["bedrock", "oauth-a", "oauth-b"]
spend_down_within = "25%"

# 消費の上限 (DR-0019) を見るための面。oauth-a にだけ上限を掛け、
# 上限に当たったときに oauth-b へ落ちることを見る。
[[ns.paced.routing]]
models = ["claude-sonnet-5"]
routes = [
    { route = "oauth-a", pace_cap = { ratio = "100%", step = "1d" } },
    "oauth-b",
]

# 逃げ場が無い面。上限に当たった経路しか無いときの答えを見る。
[[ns.paced-only.routing]]
models = ["claude-sonnet-5"]
routes = [{ route = "oauth-a", pace_cap = { ratio = "100%", step = "1d" } }]

# 同格グループの中に上限を書いた面 (DR-0019 §1)。使い切りの繰り上げも
# 効かせて、上限で外れた経路が昇格しないことを見る (DR-0019 §8)。
[[ns.paced-group.routing]]
models = ["claude-sonnet-5"]
routes = [
    ["bedrock", { route = "oauth-a", pace_cap = { ratio = "100%", step = "1d" } }],
    "oauth-b",
]
spend_down_within = "25%"

# 上限つきの経路を後ろに置いた面。使い切りの繰り上げが効く配置なので、
# 「上限で外れる経路は昇格しない」が順序に出る (DR-0019 §8)。
[[ns.paced-order.routing]]
models = ["claude-sonnet-5"]
routes = [
    "bedrock",
    { route = "oauth-b", pace_cap = { ratio = "100%", step = "1d" } },
]
spend_down_within = "25%"
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
        r.config.namespace(NS).expect("the default always exists")
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
            .routes_for_at(ns(&r), NS, "claude-fable-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a"]);
    }

    fn observe_seven_day_reset(r: &Router, route: &str, reset: i64) {
        let snapshot = crate::quota::Snapshot::new(
            NOW,
            None,
            Some(crate::quota::Window::default().with_reset(Some(reset))),
            None,
        )
        .unwrap();
        r.preset(route).unwrap().restore_quota(snapshot);
    }

    /// 同格グループでは、未来にある 7 日枠 reset が近い経路から試す。
    #[tokio::test]
    async fn equal_group_prefers_the_nearest_future_seven_day_reset() {
        let r = router().await;
        observe_seven_day_reset(&r, "bedrock", NOW + 200);
        observe_seven_day_reset(&r, "oauth-a", NOW + 100);
        let got = r
            .routes_for_at(ns(&r), NS, "claude-fable-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["oauth-a", "bedrock"]);
    }

    /// reset が読めない経路は、有効な reset を持つ経路の後ろへ送る。
    #[tokio::test]
    async fn equal_group_puts_unknown_reset_after_future_reset() {
        let r = router().await;
        observe_seven_day_reset(&r, "oauth-a", NOW + 100);
        let got = r
            .routes_for_at(ns(&r), NS, "claude-fable-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["oauth-a", "bedrock"]);
    }

    /// 現在以前の reset は陳腐化しているため、有効な reset の後ろへ送る。
    #[tokio::test]
    async fn equal_group_puts_stale_reset_after_future_reset() {
        let r = router().await;
        observe_seven_day_reset(&r, "bedrock", NOW);
        observe_seven_day_reset(&r, "oauth-a", NOW + 100);
        let got = r
            .routes_for_at(ns(&r), NS, "claude-fable-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["oauth-a", "bedrock"]);
    }

    /// 全経路の reset が不明なら、安定ソートによりグループ内記載順を維持する。
    #[tokio::test]
    async fn equal_group_keeps_declared_order_when_all_resets_are_unknown() {
        let r = router().await;
        let got = r
            .routes_for_at(ns(&r), NS, "claude-fable-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a"]);
    }

    /// フラット配列は単独グループ列なので、quota に関係なく従来の優先順を維持する。
    #[tokio::test]
    async fn flat_routes_keep_declared_priority_despite_reset_times() {
        let r = router().await;
        observe_seven_day_reset(&r, "bedrock", NOW + 200);
        observe_seven_day_reset(&r, "oauth-a", NOW + 100);
        observe_seven_day_reset(&r, "oauth-b", NOW + 50);
        let got = r
            .routes_for_at(ns(&r), NS, "claude-sonnet-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a", "oauth-b"]);
    }

    // ---------- 使い切りの繰り上げ (DR-0018) ----------

    const WEEK: u64 = 7 * 24 * 60 * 60;
    /// 7d 窓の 25 % = 42 時間。閾値の内と外はこの値をまたぐかで決まる。
    const QUARTER_OF_A_WEEK: i64 = 42 * 60 * 60;
    /// 使い切りを設定してある面。
    const SPEND_NS: &str = "spend";

    fn spend_ns(r: &Router) -> &Namespace {
        r.config.namespace(SPEND_NS).expect("declared in CONFIG")
    }

    /// 周期と reset を申告した窓を 1 つ持たせる。
    fn observe_window(r: &Router, route: &str, window_seconds: Option<u64>, reset: Option<i64>) {
        let window = crate::quota::Window::default()
            .with_reset(reset)
            .with_window_seconds(window_seconds);
        let snapshot = crate::quota::Snapshot::new(NOW, None, Some(window), None).unwrap();
        r.preset(route).unwrap().restore_quota(snapshot);
    }

    async fn spend_order(r: &Router) -> Vec<String> {
        r.routes_for_at(
            spend_ns(r),
            SPEND_NS,
            "claude-sonnet-5",
            &session("s1"),
            NOW,
        )
        .await
        .unwrap()
        .iter()
        .map(|route| route.name().to_owned())
        .collect()
    }

    /// 閾値内にリセットが迫った経路は、静的順を飛び越えて先頭へ来る。
    #[tokio::test]
    async fn spend_down_promotes_a_route_whose_window_is_about_to_reset() {
        let r = router().await;
        observe_window(&r, "oauth-b", Some(WEEK), Some(NOW + QUARTER_OF_A_WEEK - 1));
        assert_eq!(spend_order(&r).await, ["oauth-b", "bedrock", "oauth-a"]);
    }

    /// 閾値の外なら静的順のまま。繰り上げは「間際」に限る。
    #[tokio::test]
    async fn spend_down_leaves_a_distant_reset_alone() {
        let r = router().await;
        observe_window(&r, "oauth-b", Some(WEEK), Some(NOW + QUARTER_OF_A_WEEK + 1));
        assert_eq!(spend_order(&r).await, ["bedrock", "oauth-a", "oauth-b"]);
    }

    /// 複数が閾値内なら、リセットが近い方から使い切る。
    #[tokio::test]
    async fn spend_down_orders_promotions_by_the_nearest_reset() {
        let r = router().await;
        observe_window(&r, "bedrock", Some(WEEK), Some(NOW + 3600));
        observe_window(&r, "oauth-b", Some(WEEK), Some(NOW + 600));
        assert_eq!(spend_order(&r).await, ["oauth-b", "bedrock", "oauth-a"]);
    }

    /// 割合は窓長に掛かる。5 時間しか回らない窓では 25 % も 1 時間 15 分。
    #[tokio::test]
    async fn spend_down_scales_a_percentage_to_the_window_length() {
        let r = router().await;
        const FIVE_HOURS: u64 = 5 * 60 * 60;
        // 7d 窓なら余裕で閾値内だが、5h 窓の 25 % (75 分) には届かない。
        observe_window(&r, "oauth-b", Some(FIVE_HOURS), Some(NOW + 76 * 60));
        assert_eq!(spend_order(&r).await, ["bedrock", "oauth-a", "oauth-b"]);

        observe_window(&r, "oauth-b", Some(FIVE_HOURS), Some(NOW + 74 * 60));
        assert_eq!(spend_order(&r).await, ["oauth-b", "bedrock", "oauth-a"]);
    }

    /// 見るのは周期が一番長い窓だけ。短い窓の間際は繰り上げの理由にしない。
    #[tokio::test]
    async fn spend_down_looks_only_at_the_longest_window() {
        let r = router().await;
        let snapshot = crate::quota::Snapshot::new(
            NOW,
            // 5h 窓はもうすぐ回るが、これは数時間で戻るので蒸発の損が無い。
            Some(
                crate::quota::Window::default()
                    .with_reset(Some(NOW + 60))
                    .with_window_seconds(Some(5 * 60 * 60)),
            ),
            Some(
                crate::quota::Window::default()
                    .with_reset(Some(NOW + QUARTER_OF_A_WEEK + 1))
                    .with_window_seconds(Some(WEEK)),
            ),
            None,
        )
        .unwrap();
        r.preset("oauth-b").unwrap().restore_quota(snapshot);

        assert_eq!(spend_order(&r).await, ["bedrock", "oauth-a", "oauth-b"]);
    }

    /// 窓長かリセット時刻が取れない経路は動かさない。推測で順位を上げない。
    #[tokio::test]
    async fn spend_down_skips_a_route_it_cannot_read() {
        let r = router().await;
        // 周期は分かるが、いつ回るか分からない。
        observe_window(&r, "oauth-a", Some(WEEK), None);
        // すぐ回ると分かるが、それが何日周期の枠か分からない。
        observe_window(&r, "oauth-b", None, Some(NOW + 60));
        assert_eq!(spend_order(&r).await, ["bedrock", "oauth-a", "oauth-b"]);
    }

    /// 過ぎたリセット時刻は陳腐化した観測なので、繰り上げの根拠にしない。
    #[tokio::test]
    async fn spend_down_ignores_a_reset_already_past() {
        let r = router().await;
        observe_window(&r, "oauth-b", Some(WEEK), Some(NOW));
        assert_eq!(spend_order(&r).await, ["bedrock", "oauth-a", "oauth-b"]);
    }

    /// 設定に書かない面では、リセットが間際でも順序は動かない。
    #[tokio::test]
    async fn spend_down_does_nothing_without_the_setting() {
        let r = router().await;
        observe_window(&r, "oauth-b", Some(WEEK), Some(NOW + 60));
        let got = r
            .routes_for_at(ns(&r), NS, "claude-sonnet-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&got), vec!["bedrock", "oauth-a", "oauth-b"]);
    }

    /// 会話に貼り付いた経路の方が繰り上げより強い。
    ///
    /// 途中で credential が変わると prompt cache が切れる。使い切りで拾う枠
    /// より、積み直しの消費の方が大きい (DR-0018 §3)。
    #[tokio::test]
    async fn session_affinity_outranks_the_spend_down_promotion() {
        let r = router().await;
        let session = session("s1");
        // bedrock に貼り付いた会話を作る。
        let routes = r
            .routes_for_at(spend_ns(&r), SPEND_NS, "claude-sonnet-5", &session, NOW)
            .await
            .unwrap();
        r.remember(SPEND_NS, &session, "claude-sonnet-5", &routes[0])
            .await;
        assert_eq!(routes[0].name(), "bedrock");

        // その後 oauth-b のリセットが間際になっても、先頭は貼り付いた方のまま。
        observe_window(&r, "oauth-b", Some(WEEK), Some(NOW + 60));
        let got = r
            .routes_for_at(spend_ns(&r), SPEND_NS, "claude-sonnet-5", &session, NOW)
            .await
            .unwrap();
        assert_eq!(
            names(&got),
            vec!["bedrock", "oauth-b", "oauth-a"],
            "affinity takes the front; the promotion still applies to the rest"
        );
    }

    // ---------- 消費の上限 (DR-0019) ----------

    /// 上限を設定してある面。
    const PACED_NS: &str = "paced";
    const DAY: i64 = 24 * 60 * 60;

    fn paced_ns(r: &Router) -> &Namespace {
        r.config.namespace(PACED_NS).expect("declared in CONFIG")
    }

    /// 窓の頭から `elapsed` 経ち、`utilization` まで使った状態にする。
    fn observe_usage(r: &Router, route: &str, elapsed: i64, utilization: f64) {
        let window = crate::quota::Window {
            utilization: Some(utilization),
            ..crate::quota::Window::default()
        }
        .with_reset(Some(NOW + WEEK as i64 - elapsed))
        .with_window_seconds(Some(WEEK));
        let snapshot = crate::quota::Snapshot::new(NOW, None, Some(window), None).unwrap();
        r.preset(route).unwrap().restore_quota(snapshot);
    }

    /// 上限を見たうえで今使える経路。
    async fn paced_ready(r: &Router) -> Vec<String> {
        let routes = r
            .routes_for_at(
                paced_ns(r),
                PACED_NS,
                "claude-sonnet-5",
                &session("s1"),
                NOW,
            )
            .await
            .unwrap();
        match r.select(&routes, "claude-sonnet-5", NOW, &origin("claude-sonnet-5")) {
            Selection::Ready(ready) => ready.iter().map(|r| r.name().to_owned()).collect(),
            Selection::AllDenied { .. } => Vec::new(),
        }
    }

    /// 経過ぶんに収まっている経路はそのまま使える。
    #[tokio::test]
    async fn a_route_within_its_pace_stays_available() {
        let r = router().await;
        // 3 日経って 3/7 弱。予算 (3/7) に届いていない。
        observe_usage(&r, "oauth-a", 3 * DAY, 3.0 / 7.0 - 0.01);
        observe_usage(&r, "oauth-b", 3 * DAY, 0.99);
        assert_eq!(paced_ready(&r).await, ["oauth-a", "oauth-b"]);
    }

    /// 経過ぶんを超えて使った経路は候補から外れ、上限の無い経路へ落ちる。
    #[tokio::test]
    async fn a_route_over_its_pace_is_held_back() {
        let r = router().await;
        // 3 日で 5/7 まで使った。予算は 3/7 なので使い過ぎ。
        observe_usage(&r, "oauth-a", 3 * DAY, 5.0 / 7.0);
        observe_usage(&r, "oauth-b", 3 * DAY, 5.0 / 7.0);
        assert_eq!(
            paced_ready(&r).await,
            ["oauth-b"],
            "the capped route steps aside; the uncapped one does not"
        );
    }

    /// 外した経路が戻るのは次の段。予算が増えるまで開かない。
    #[tokio::test]
    async fn a_held_route_reopens_at_the_next_step() {
        let r = router().await;
        let routes = r
            .routes_for_at(
                paced_ns(&r),
                PACED_NS,
                "claude-sonnet-5",
                &session("s1"),
                NOW,
            )
            .await
            .unwrap();
        let capped = routes.iter().find(|r| r.name() == "oauth-a").unwrap();

        // 3 日と半日経過。次の段は 4 日目の頭なので、あと半日。
        observe_usage(&r, "oauth-a", 3 * DAY + DAY / 2, 1.0);
        let held = capped.paced_out(NOW).expect("over its pace");
        assert_eq!(held.until, NOW + DAY / 2);
        assert_eq!(held.reason, crate::denial::Reason::Paced);
    }

    /// 窓の頭では予算がまだ無い。1 段目に上がるまで一切使わせない。
    #[tokio::test]
    async fn nothing_may_be_spent_before_the_first_step() {
        let r = router().await;
        observe_usage(&r, "oauth-a", 60, 0.001);
        observe_usage(&r, "oauth-b", 60, 0.001);
        assert_eq!(paced_ready(&r).await, ["oauth-b"]);
    }

    /// 上限を書いていない面では、同じ使用率でも素通しする。
    #[tokio::test]
    async fn an_uncapped_namespace_ignores_the_usage() {
        let r = router().await;
        observe_usage(&r, "oauth-a", 60, 1.0);
        let routes = r
            .routes_for_at(ns(&r), NS, "claude-sonnet-5", &session("s1"), NOW)
            .await
            .unwrap();
        let Selection::Ready(ready) = r.select(&routes, "claude-sonnet-5", NOW, &origin("m"))
        else {
            panic!("nothing is denied without a cap");
        };
        assert_eq!(names(&ready), vec!["bedrock", "oauth-a", "oauth-b"]);
    }

    /// 枠が読めない上限つき経路は通さず、聞き直しの合図を立てる。
    #[tokio::test]
    async fn a_capped_route_with_no_quota_is_closed_and_asks() {
        let r = router().await;
        observe_usage(&r, "oauth-b", 3 * DAY, 0.1);
        let routes = r
            .routes_for_at(
                paced_ns(&r),
                PACED_NS,
                "claude-sonnet-5",
                &session("s1"),
                NOW,
            )
            .await
            .unwrap();

        let capped = routes.iter().find(|r| r.name() == "oauth-a").unwrap();
        assert!(capped.needs_quota(NOW), "nothing is known about its usage");
        assert_eq!(
            capped.paced_out(NOW).map(|held| held.until),
            Some(NOW + crate::denial::DEFAULT_BACKOFF),
            "closed briefly, long enough to ask"
        );
        assert_eq!(paced_ready(&r).await, ["oauth-b"]);

        // 上限を書いていない経路は、枠が読めなくても聞き直しの対象ではない。
        let plain = routes.iter().find(|r| r.name() == "oauth-b").unwrap();
        assert!(!plain.needs_quota(NOW));
    }

    /// 使用率だけ読めても、いつ回る窓か分からなければ判定できない。
    #[tokio::test]
    async fn a_capped_route_without_a_window_length_is_closed() {
        let r = router().await;
        let window = crate::quota::Window {
            utilization: Some(0.1),
            ..crate::quota::Window::default()
        }
        .with_reset(Some(NOW + WEEK as i64));
        let snapshot = crate::quota::Snapshot::new(NOW, None, Some(window), None).unwrap();
        r.preset("oauth-a").unwrap().restore_quota(snapshot);
        observe_usage(&r, "oauth-b", 3 * DAY, 0.1);

        assert_eq!(paced_ready(&r).await, ["oauth-b"]);
    }

    /// 全部が上限に当たったら、既存の全滅の答え (429) がそのまま出る。
    #[tokio::test]
    async fn all_routes_held_back_answer_like_any_other_denial() {
        let r = router().await;
        observe_usage(&r, "oauth-a", 3 * DAY, 1.0);

        const ONLY_NS: &str = "paced-only";
        let ns = r.config.namespace(ONLY_NS).expect("declared in CONFIG");
        let routes = r
            .routes_for_at(ns, ONLY_NS, "claude-sonnet-5", &session("s1"), NOW)
            .await
            .unwrap();
        assert_eq!(names(&routes), vec!["oauth-a"], "no fallback exists");
        let Selection::AllDenied { until, .. } =
            r.select(&routes, "claude-sonnet-5", NOW, &origin("claude-sonnet-5"))
        else {
            panic!("the only route is over its pace");
        };
        assert_eq!(until, NOW + DAY, "reopens when the next step arrives");
    }

    /// 予算ちょうどまでは使ってよい。超えたぶんだけが外れる。
    #[tokio::test]
    async fn spending_exactly_the_budget_is_still_allowed() {
        let r = router().await;
        observe_usage(&r, "oauth-b", 3 * DAY, 0.1);

        // 3 日ぶんの予算ちょうど。
        observe_usage(&r, "oauth-a", 3 * DAY, 3.0 / 7.0);
        assert_eq!(paced_ready(&r).await, ["oauth-a", "oauth-b"]);

        // ほんの少し超えると外れる。
        observe_usage(&r, "oauth-a", 3 * DAY, 3.0 / 7.0 + 1e-6);
        assert_eq!(paced_ready(&r).await, ["oauth-b"]);
    }

    /// 同格グループの中に書いた上限も効く (DR-0019 §1)。
    #[tokio::test]
    async fn a_pace_cap_inside_an_equal_group_applies() {
        let r = router().await;
        const GROUP_NS: &str = "paced-group";
        let ns = r.config.namespace(GROUP_NS).expect("declared in CONFIG");

        // グループの相手 (bedrock) と外の経路は素通し。oauth-a だけ使い過ぎ。
        observe_usage(&r, "bedrock", 3 * DAY, 1.0);
        observe_usage(&r, "oauth-b", 3 * DAY, 1.0);
        observe_usage(&r, "oauth-a", 3 * DAY, 1.0);

        let routes = r
            .routes_for_at(ns, GROUP_NS, "claude-sonnet-5", &session("s1"), NOW)
            .await
            .unwrap();
        let Selection::Ready(ready) =
            r.select(&routes, "claude-sonnet-5", NOW, &origin("claude-sonnet-5"))
        else {
            panic!("the uncapped routes are still available");
        };
        assert_eq!(
            names(&ready),
            vec!["bedrock", "oauth-b"],
            "only the capped member of the group steps aside"
        );
    }

    /// 上限で外れる経路は、使い切りの繰り上げ対象にもならない (DR-0019 §8)。
    #[tokio::test]
    async fn a_capped_out_route_is_not_promoted_by_spend_down() {
        let r = router().await;
        const ORDER_NS: &str = "paced-order";
        let ns = r.config.namespace(ORDER_NS).expect("declared in CONFIG");
        async fn order(r: &Router, ns: &Namespace) -> String {
            names(
                &r.routes_for_at(ns, "paced-order", "claude-sonnet-5", &session("s1"), NOW)
                    .await
                    .unwrap(),
            )
            .join(",")
        }

        // oauth-b のリセットは間近 (残り 1 日 = 7d 窓の 25 % 以内)。
        // 按分線に収まっているうちは、使い切りのために先頭へ繰り上がる。
        observe_usage(&r, "bedrock", 0, 0.0);
        observe_usage(&r, "oauth-b", 6 * DAY, 6.0 / 7.0);
        assert_eq!(order(&r, ns).await, "oauth-b,bedrock");

        // 按分線を超えると、繰り上げの対象から外れて元の位置へ戻る。
        observe_usage(&r, "oauth-b", 6 * DAY, 1.0);
        assert_eq!(order(&r, ns).await, "bedrock,oauth-b");
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
        assert_eq!(names(&got), vec!["oauth-a"], "oauth-b excludes haiku");
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
            "Bedrock is namespaced"
        );
        assert_eq!(routes[1].upstream_model, None, "official stays as-is");
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
            "hands out the same instance"
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
            .reject(429, &Headers::default(), None, "claude-fable-5", NOW)
            .expect("429 is denied");

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
            .reject(429, &Headers::default(), None, "claude-fable-5", NOW);

        let Selection::Ready(ready) =
            r.select(&routes, "claude-fable-5", NOW, &origin("claude-fable-5"))
        else {
            panic!("one should remain");
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
                .reject(429, &Headers::default(), None, model, NOW)
                .expect("429 is denied");
        }

        let Selection::AllDenied { response, until } =
            r.select(&routes, model, NOW, &origin(model))
        else {
            panic!("should be all denied");
        };

        assert_eq!(response.status, 429);
        assert_eq!(until, NOW + 60, "the earliest reopen time");
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
            "returned in a shape the client can read"
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
            route
                .preset
                .reject(429, &Headers::default(), None, model, NOW);
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
            "the gateway itself answered, not any credential"
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
        assert_eq!(again.len(), 2, "candidates are not reduced");
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
            "only the pinned one is affected"
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
            "has no effect on a different conversation"
        );
        assert_eq!(
            names(&r.routes_for(ns(&r), NS, "claude-opus-5", &s).await.unwrap()),
            vec!["oauth-a", "oauth-b"],
            "has no effect on a different model"
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
        let other = r.config.namespace("other").expect("present in config");

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
            "applies within the remembered namespace"
        );
        assert_eq!(
            names(
                &r.routes_for(ns(&r), NS, "claude-fable-5", &s)
                    .await
                    .unwrap()
            ),
            vec!["bedrock", "oauth-a"],
            "a different namespace's order is unaffected even by the same conversation key"
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
            "first hop"
        );
        assert_eq!(
            r.resolve(ns(&r), "opus").await,
            "claude-opus-5",
            "second hop"
        );
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
        assert!(!resolved.contains_key("a"), "a cycle is dropped");
        assert!(!resolved.contains_key("b"));
        assert_eq!(
            resolved["ok"], "claude-opus-5",
            "others are not caught up in it"
        );
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

    /// 一覧が空 (200 で空配列を返すアカウントは実在する) でも、宣言された
    /// モデルは公開される。宣言も無い経路は空のまま。
    #[test]
    fn an_empty_listing_falls_back_to_declared_models() {
        let config: Config = toml::from_str(CONFIG).unwrap();
        let declared = &config.routes["cpa"]; // models = ["gpt-5.6-sol"] を宣言
        let bare = &config.routes["oauth-a"]; // 宣言なし

        let filled = with_declared_fallback(BTreeMap::new(), declared);
        assert_eq!(
            filled.get("gpt-5.6-sol").map(String::as_str),
            Some("gpt-5.6-sol")
        );

        assert!(with_declared_fallback(BTreeMap::new(), bare).is_empty());

        // 一覧が取れた経路はそのまま。宣言に上書きされない。
        let found: BTreeMap<_, _> = [("a".to_owned(), "up.a".to_owned())].into();
        assert_eq!(with_declared_fallback(found.clone(), declared), found);
    }
}
