//! provider preset の core 契約。
//!
//! provider は一枚岩の trait ではなく、認証・wire・metering・任意の枠照会を
//! 小さな trait として組み合わせる。Bedrock のように認証だけ差し替え、wire を
//! 再利用する構成を委譲で表せる。

use std::sync::Arc;

use crate::Result;
use crate::credential::Credential;
use crate::denial::Denial;
use crate::egress::{BoxFuture, EgressRequest, Headers, Response, UpstreamRequest};
use crate::metering::{Pricing, UsageObserver};
use crate::quota::{QuotaLimit, Snapshot};

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

/// トークンを消費せず quota を問い合わせる任意 capability。
pub trait QuotaApi: Send + Sync {
    fn fetch<'a>(
        &'a self,
        http: &'a reqwest::Client,
        credential: &'a Credential,
    ) -> BoxFuture<'a, Result<Vec<QuotaLimit>>>;
}

/// 1 経路を構成する provider 実装の束。
///
/// capability が無いことは `Option` で表し、空実装や「未対応」エラーを置かない。
pub struct Preset {
    name: String,
    auth: Arc<dyn Auth>,
    wire: Arc<dyn Wire>,
    metering: Arc<dyn Metering>,
    quota_api: Option<Arc<dyn QuotaApi>>,
}

impl Preset {
    pub fn new(
        name: impl Into<String>,
        auth: Arc<dyn Auth>,
        wire: Arc<dyn Wire>,
        metering: Arc<dyn Metering>,
        quota_api: Option<Arc<dyn QuotaApi>>,
    ) -> Self {
        Self {
            name: name.into(),
            auth,
            wire,
            metering,
            quota_api,
        }
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
    use crate::egress::BodyStream;
    use crate::metering::TokenUsage;

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

    struct NoMetering;

    impl Metering for NoMetering {
        fn quota_snapshot(&self, _headers: &Headers, _observed_at: i64) -> Option<Snapshot> {
            None
        }

        fn rejection(
            &self,
            _status: u16,
            _headers: &Headers,
            _model: &str,
            _observed_at: i64,
        ) -> Option<Denial> {
            None
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

    /// capability を持たない preset は quota_api が `None`。呼んでから未対応を
    /// 返す空実装ではなく、呼び出す能力自体が無いことを型で表す。
    #[test]
    fn optional_capability_is_absent_by_type() {
        let preset = Preset::new(
            "passthrough",
            Arc::new(NoAuth),
            Arc::new(StubWire),
            Arc::new(NoMetering),
            None,
        );

        assert_eq!(preset.name(), "passthrough");
        assert!(preset.quota_api().is_none());
    }

    /// UsageObserver は trait object として chunk を受け、終端で結果を返せる。
    #[test]
    fn usage_observer_is_object_safe() {
        let mut observer: Box<dyn UsageObserver> = Box::new(Observer);
        observer.observe(b"data");
        assert_eq!(observer.finish(), None);
    }
}
