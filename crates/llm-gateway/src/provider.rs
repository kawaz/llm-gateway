//! provider preset の core 契約。
//!
//! provider は一枚岩の trait ではなく、認証・wire・metering・任意の capability を
//! 小さな trait として組み合わせる。認証だけ差し替えて wire を再利用する構成を、
//! 継承もどきでなく委譲で表せる。
//!
//! 束ねた [`Preset`] は **1 経路の状態も持つ** (DR-0014 §3)。断られた印・
//! 枠のスナップショット・様子見の予定は、その意味を知っている provider の
//! 側にある。router は経路の名前を鍵に map を引くのではなく、経路そのものへ
//! 「今使えるか」を聞く。

use std::sync::Arc;

use crate::Result;
use crate::credential::Credential;
use crate::denial::{Availability, Denial, Probing, RouteState, Scope};
use crate::egress::{BoxFuture, EgressRequest, Headers, Response, UpstreamRequest};
use crate::metering::{Pricing, UsageObserver};
use crate::quota::{QuotaLimit, Snapshot, Support};

/// request-time の認証を適用する。
///
/// login/refresh lifecycleはP3で追加予定（現状credential/が所有）。
pub trait Auth: Send + Sync {
    fn authorize(
        &self,
        credential: Option<&Credential>,
        request: &mut UpstreamRequest,
    ) -> Result<()>;
}

/// 正規形を upstream 方言へ変換して送る。
pub trait Wire: Send + Sync {
    fn encode(&self, request: EgressRequest) -> Result<UpstreamRequest>;

    fn send<'a>(
        &'a self,
        http: &'a reqwest::Client,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<Response>>;
}

/// 応答から provider 固有の quota・拒否・usage・単価を読む。
pub trait Metering: Send + Sync {
    /// 応答ヘッダに quota が載っていれば正規スナップショットへ写す。
    fn quota_snapshot(&self, headers: &Headers, observed_at: i64) -> Option<Snapshot>;

    /// この応答が一時的な経路拒否なら、候補へ戻す条件を返す。
    fn rejection(
        &self,
        status: u16,
        headers: &Headers,
        model: &str,
        observed_at: i64,
    ) -> Option<Denial>;

    /// 本文 usage を読む observer。読めない content-type なら `None`。
    fn usage_observer(&self, content_type: Option<&str>) -> Option<Box<dyn UsageObserver>>;

    /// このモデルに適用する非重複の課金軸。値付けできなければ `None`。
    fn pricing(&self, model: &str) -> Option<Pricing>;
}

/// 枠を能動的に確かめる任意 capability。
///
/// 確かめ方は 2 通りある。トークンを使わない照会 ([`Self::fetch`]) と、応答
/// ヘッダに枠を載せさせるための最小リクエスト ([`Self::probe_request`])。
/// 後者が要るのは、枠ヘッダを得る手立てが実リクエストしかないため (DR-0007)。
/// 両方を 1 つの capability に収めるのは、どちらも「この upstream に枠という
/// 概念があり、聞く手立てがある」ことを前提にしているため。
pub trait QuotaApi: Send + Sync {
    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<QuotaLimit>>>;

    /// 聞けた枠を、範囲ごとの締め出し指示へ読み解く。
    ///
    /// `Some` はその範囲を締め出す印、`None` はその範囲を開ける指示。どの枠が
    /// どのモデル群に掛かるかは upstream の語彙の話なので provider が読む。
    fn denials(&self, limits: &[QuotaLimit], now: i64) -> Vec<(Scope, Option<Denial>)>;

    /// 枠ヘッダを引き出すための最小リクエスト。
    ///
    /// 何を投げれば消費が最小で済むか (どのモデルに、何を頼むか) は方言の
    /// 知識なので provider が組む。投げるのは呼び出し側。
    fn probe_request(&self) -> ProbeRequest;
}

/// 枠ヘッダを引き出すための 1 本 (DR-0007)。
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    /// 使うモデル。何をどれだけ消費したかの報告に出す。
    pub model: String,
    /// 送る中身。正規形のまま渡し、方言への変換は Wire が担う。
    pub request: EgressRequest,
}

/// upstream が受け付けない要求ヘッダを、送る前に落として学習する任意 capability。
///
/// DR-0003 の beta フラグ交渉がこれ。「何を載せたか」を返すのは、400 を受けた
/// ときに何を責めるかがそれで決まるため。provider 1 つでは完結せず、学習した
/// 分の保存は credential 側にあるので、往復の司会は転送側が持つ。
pub trait Negotiation: Send + Sync {
    /// 送るヘッダを整える。実際に載せたものを返す。
    ///
    /// `learned` はこの credential で拒否されたと分かっている分。
    fn prepare(&self, headers: &mut Headers, learned: &[String]) -> Vec<String>;

    /// この失敗応答は、載せたもののせいか。そうなら責めるものの名前。
    fn blame(&self, body: &str, sent: &[String]) -> Option<Vec<String>>;
}

/// 1 経路を構成する provider 実装の束と、その経路の状態。
///
/// capability が無いことは `Option` で表し、空実装や「未対応」エラーを置かない。
pub struct Preset {
    name: String,
    auth: Arc<dyn Auth>,
    wire: Arc<dyn Wire>,
    metering: Arc<dyn Metering>,
    quota_api: Option<Arc<dyn QuotaApi>>,
    negotiation: Option<Arc<dyn Negotiation>>,
    /// まだ何も観測していないときに、枠について言えること。
    unobserved: Support,
    state: RouteState,
}

impl Preset {
    /// 必須の 3 つを束ねる。任意の capability は `with_*` で足す。
    pub fn new(
        name: impl Into<String>,
        auth: Arc<dyn Auth>,
        wire: Arc<dyn Wire>,
        metering: Arc<dyn Metering>,
    ) -> Self {
        Self {
            name: name.into(),
            auth,
            wire,
            metering,
            quota_api: None,
            negotiation: None,
            // 何も宣言しない経路について言えることは無い。「取れない」と
            // 断じるのも「まだ観測していない」と言うのも、こちらの推測になる。
            unobserved: Support::UpstreamDependent,
            state: RouteState::new(),
        }
    }

    pub fn with_quota_api(mut self, quota_api: Arc<dyn QuotaApi>) -> Self {
        self.quota_api = Some(quota_api);
        self
    }

    /// 観測前に枠について言えることを宣言する。
    ///
    /// 「取れるがまだ観測していない」のか「仕組みとして取れない」のかは、
    /// その upstream を知っている側にしか決められない。閲覧の口はこの値を
    /// そのまま出す (DR-0007)。
    pub fn with_quota_support(mut self, unobserved: Support) -> Self {
        self.unobserved = unobserved;
        self
    }

    pub fn with_negotiation(mut self, negotiation: Arc<dyn Negotiation>) -> Self {
        self.negotiation = Some(negotiation);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn auth(&self) -> &dyn Auth {
        self.auth.as_ref()
    }

    pub fn wire(&self) -> &dyn Wire {
        self.wire.as_ref()
    }

    pub fn metering(&self) -> &dyn Metering {
        self.metering.as_ref()
    }

    pub fn quota_api(&self) -> Option<&dyn QuotaApi> {
        self.quota_api.as_deref()
    }

    pub fn negotiation(&self) -> Option<&dyn Negotiation> {
        self.negotiation.as_deref()
    }

    /// このモデルを今この経路へ流せるか。
    pub fn availability(&self, model: &str, now: i64) -> Availability {
        self.state.availability(model, now)
    }

    /// 通った。このモデルに効いていた印を外す。
    pub fn allow(&self, model: &str) {
        self.state.allow(model);
    }

    /// 断られた応答を読ませて、締め出す価値があれば印を付ける。
    ///
    /// 読み方は provider の [`Metering`] が知っている。返すのは付けた印で、
    /// 呼び出し側が理由と期限をログに出すのに使う。
    pub fn reject(&self, status: u16, headers: &Headers, model: &str, now: i64) -> Option<Denial> {
        let denial = self.metering.rejection(status, headers, model, now)?;
        self.state.deny(denial.clone(), now);
        Some(denial)
    }

    /// 応答ヘッダに枠が載っていれば控える。控えた分を返す。
    pub fn observe_quota(&self, headers: &Headers, now: i64) -> Option<Snapshot> {
        let snapshot = self.metering.quota_snapshot(headers, now)?;
        self.state.observe_quota(snapshot.clone());
        Some(snapshot)
    }

    /// 前回の観測を読み戻す (起動時)。
    pub fn restore_quota(&self, snapshot: Snapshot) {
        self.state.observe_quota(snapshot);
    }

    /// 最後に観測した枠。
    pub fn quota(&self) -> Option<Snapshot> {
        self.state.quota()
    }

    /// この経路の枠について、今どこまで言えるか。
    ///
    /// 観測があればそれが答え。無いときに何と言うかは preset の宣言
    /// ([`Self::with_quota_support`]) に従う。
    pub fn quota_support(&self) -> Support {
        match self.quota() {
            Some(_) => Support::Observed,
            None => self.unobserved,
        }
    }

    /// 枠ヘッダを引き出す最小リクエスト。枠を聞ける経路だけが持つ。
    pub fn probe_request(&self) -> Option<ProbeRequest> {
        Some(self.quota_api()?.probe_request())
    }

    /// 枠照会 API の答えで印を引き直す。
    pub fn apply_quota(&self, limits: &[QuotaLimit], now: i64) {
        let Some(api) = self.quota_api() else {
            return;
        };
        self.state.apply(&api.denials(limits, now), now);
    }

    /// 定期の様子見の役を引き受ける。
    ///
    /// 枠照会 API を持たない経路では引き受けない — 札を取っても聞く相手が
    /// いない。「無い」は型に出ているので、設定の種別を見る必要もない。
    pub fn claim_probe(&self, now: i64) -> Option<Probing> {
        self.quota_api.as_ref()?;
        self.state.claim_probe(now)
    }

    /// 間隔を待たずに様子見の役を引き受ける。
    pub fn claim_ask(&self, now: i64) -> Option<Probing> {
        self.quota_api.as_ref()?;
        self.state.claim_ask(now)
    }

    /// 試験から任意の印を置く。
    ///
    /// 応答から読める形に限らない締め出し (期限の切れた印など) を作るため。
    #[cfg(test)]
    pub(crate) fn deny(&self, denial: Denial, now: i64) {
        self.state.deny(denial, now);
    }
}

impl std::fmt::Debug for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preset")
            .field("name", &self.name)
            .field("quota_api", &self.quota_api.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::denial::Reason;
    use crate::egress::BodyStream;
    use crate::metering::TokenUsage;

    const NOW: i64 = 1_800_000_000;

    struct NoAuth;

    impl Auth for NoAuth {
        fn authorize(
            &self,
            _credential: Option<&Credential>,
            _request: &mut UpstreamRequest,
        ) -> Result<()> {
            Ok(())
        }
    }

    struct StubWire;

    impl Wire for StubWire {
        fn encode(&self, request: EgressRequest) -> Result<UpstreamRequest> {
            Ok(UpstreamRequest {
                url: request.path,
                headers: request.headers,
                body: bytes::Bytes::from(serde_json::to_vec(&request.body)?),
            })
        }

        fn send<'a>(
            &'a self,
            _http: &'a reqwest::Client,
            _request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<Response>> {
            Box::pin(async {
                let body: BodyStream = Box::pin(futures_util::stream::empty());
                Ok(Response {
                    status: 200,
                    headers: Headers::default(),
                    body,
                })
            })
        }
    }

    /// 429 だけを「60 秒だけこのモデルを外す」と読む metering。
    struct StubMetering;

    impl Metering for StubMetering {
        fn quota_snapshot(&self, headers: &Headers, observed_at: i64) -> Option<Snapshot> {
            let utilization = headers.get("x-used")?.parse().ok();
            Snapshot::new(
                observed_at,
                Some(crate::quota::Window {
                    utilization,
                    ..Default::default()
                }),
                None,
                None,
            )
        }

        fn rejection(
            &self,
            status: u16,
            _headers: &Headers,
            model: &str,
            observed_at: i64,
        ) -> Option<Denial> {
            (status == 429).then(|| Denial {
                until: observed_at + 60,
                reason: Reason::Busy,
                scope: Scope::Model(model.to_owned()),
            })
        }

        fn usage_observer(&self, _content_type: Option<&str>) -> Option<Box<dyn UsageObserver>> {
            None
        }

        fn pricing(&self, _model: &str) -> Option<Pricing> {
            None
        }
    }

    struct Observer;

    impl UsageObserver for Observer {
        fn observe(&mut self, _chunk: &[u8]) {}

        fn finish(self: Box<Self>) -> Option<TokenUsage> {
            None
        }
    }

    fn preset() -> Preset {
        Preset::new(
            "passthrough",
            Arc::new(NoAuth),
            Arc::new(StubWire),
            Arc::new(StubMetering),
        )
    }

    /// 枠を聞ける口を持つ経路。聞いた答えは常に空で、様子見の札の有無だけを見る。
    struct StubQuotaApi;

    impl QuotaApi for StubQuotaApi {
        fn fetch<'a>(
            &'a self,
            _http: &'a reqwest::Client,
            _credential: &'a Credential,
        ) -> BoxFuture<'a, Result<Vec<QuotaLimit>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn denials(&self, _limits: &[QuotaLimit], _now: i64) -> Vec<(Scope, Option<Denial>)> {
            Vec::new()
        }

        fn probe_request(&self) -> ProbeRequest {
            ProbeRequest {
                model: "probe-model".to_owned(),
                request: EgressRequest {
                    path: "/v1/messages".to_owned(),
                    query: None,
                    body: serde_json::json!({"model": "probe-model"}),
                    headers: Headers::default(),
                },
            }
        }
    }

    /// capability を持たない preset は quota_api が `None`。呼んでから未対応を
    /// 返す空実装ではなく、呼び出す能力自体が無いことを型で表す。
    #[test]
    fn optional_capability_is_absent_by_type() {
        let preset = preset();

        assert_eq!(preset.name(), "passthrough");
        assert!(preset.quota_api().is_none());
        assert!(preset.negotiation().is_none());
    }

    /// 枠を聞ける経路だけが、枠ヘッダを引き出す 1 本を組める。
    ///
    /// 聞く相手がいない経路で最小リクエストを投げても、消費するだけで何も
    /// 読めない。「無い」を型で示すので、設定の種別を見る必要もない。
    #[test]
    fn only_a_route_that_can_answer_offers_a_probe() {
        assert!(preset().probe_request().is_none());

        let asking = preset().with_quota_api(Arc::new(StubQuotaApi));
        let probe = asking.probe_request().expect("聞ける経路は 1 本組める");

        assert_eq!(probe.model, "probe-model");
        assert_eq!(probe.request.body["model"], "probe-model");
    }

    /// 観測前に何と言うかは preset が宣言し、観測が付いたらそちらが勝つ。
    #[test]
    fn what_can_be_said_about_quota_comes_from_the_route() {
        let silent = preset();
        assert_eq!(
            silent.quota_support(),
            crate::quota::Support::UpstreamDependent,
            "宣言の無い経路について言えることは無い"
        );

        let declared = preset().with_quota_support(crate::quota::Support::NotApplicable);
        assert_eq!(
            declared.quota_support(),
            crate::quota::Support::NotApplicable
        );

        let headers = Headers::new(vec![("x-used".to_owned(), "0.5".to_owned())]);
        declared.observe_quota(&headers, NOW).expect("読める");
        assert_eq!(
            declared.quota_support(),
            crate::quota::Support::Observed,
            "観測できたなら宣言より実測"
        );
    }

    /// UsageObserver は trait object として chunk を受け、終端で結果を返せる。
    #[test]
    fn usage_observer_is_object_safe() {
        let mut observer: Box<dyn UsageObserver> = Box::new(Observer);
        observer.observe(b"data");
        assert_eq!(observer.finish(), None);
    }

    /// 断られたかどうかの判断は provider が読み、印は経路が持つ。
    ///
    /// router は印の中身でなく [`Availability`] だけを受け取る。
    #[test]
    fn a_rejection_closes_the_route_it_belongs_to() {
        let preset = preset();
        assert_eq!(preset.availability("m", NOW), Availability::Ready);

        assert!(
            preset.reject(200, &Headers::default(), "m", NOW).is_none(),
            "通った応答は締め出さない"
        );
        assert!(preset.reject(429, &Headers::default(), "m", NOW).is_some());

        assert_eq!(
            preset.availability("m", NOW),
            Availability::Denied { until: NOW + 60 }
        );
        assert_eq!(
            preset.availability("other", NOW),
            Availability::Ready,
            "頼んだモデルの事情として扱う"
        );

        preset.allow("m");
        assert_eq!(preset.availability("m", NOW), Availability::Ready);
    }

    /// 印は preset ごとに独立する。経路の名前を鍵にした map ではない。
    #[test]
    fn state_does_not_leak_between_presets() {
        let one = preset();
        let other = preset();

        one.reject(429, &Headers::default(), "m", NOW);

        assert!(matches!(
            one.availability("m", NOW),
            Availability::Denied { .. }
        ));
        assert_eq!(
            other.availability("m", NOW),
            Availability::Ready,
            "同じ名前でも別の経路"
        );
    }

    /// 応答ヘッダから読めた枠は、その経路が持つ。
    #[test]
    fn the_route_keeps_the_quota_its_metering_read() {
        let preset = preset();
        assert_eq!(preset.quota(), None);

        assert!(
            preset.observe_quota(&Headers::new(vec![]), NOW).is_none(),
            "載っていない応答は観測にしない"
        );
        assert_eq!(preset.quota(), None);

        let headers = Headers::new(vec![("x-used".to_owned(), "0.5".to_owned())]);
        let snapshot = preset.observe_quota(&headers, NOW).expect("読める");
        assert_eq!(preset.quota(), Some(snapshot));
    }

    /// 枠照会 API が無ければ、様子見の札も取らない。
    #[test]
    fn a_route_without_a_quota_api_is_never_probed() {
        let preset = preset();
        preset.reject(429, &Headers::default(), "m", NOW);

        assert!(preset.claim_ask(NOW).is_none());
        assert!(
            preset
                .claim_probe(NOW + crate::denial::PROBE_INTERVAL)
                .is_none()
        );
    }
}
