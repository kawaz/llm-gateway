//! 登録した行き先への無変換の中継 (DR-0030 §2 / §4 / §5)。
//!
//! `/ns-<ns>/<name>/<rest>` で受けた 1 本を、`[upstreams.<name>]` の起点へ
//! `<rest>` をそのまま連結して流す。gateway が触るのは認証だけ — クライアントの
//! `Authorization` (と、載せ方がヘッダ指定ならそのヘッダ) を落とし、登録済みの
//! 固定の秘密を載せる。本文・その他のヘッダ・応答 (SSE を含む) は変えない
//! (DR-0025 §1 と同じ「認証だけ差し替える」形)。
//!
//! 設定に書いていない行き先・許可に無い `METHOD パス` は 404 / 405 で、上流へは
//! 問い合わせない。秘密が読めなければ 502 で、やはり上流へは出さない。

use std::collections::BTreeMap;
use std::time::Instant;

use bytes::Bytes;
use futures_util::Stream;
use gateway_core::credential::CredentialId;
use gateway_core::credential::file::FileStore;
use gateway_core::credential::secret::{StaticSecretStore, StoredSecret};
use gateway_core::upstream::{AuthPlacement, Decision, UpstreamSpec};
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

pub use gateway_core::ratelimit::Quota;

use crate::config::Config;
use crate::events::{self, Events};
use crate::{Error, Result};

/// 接続ごとの約束で、中継先へは持ち越さないヘッダ (RFC 9110 §7.6.1)。
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "transfer-encoding",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];

/// 中継しなかった理由。応答の形 (状態コードと文言) は受け口が決める。
#[derive(Debug)]
pub enum Refusal {
    /// `[upstreams.<name>]` に無い (404)。
    UnknownUpstream,
    /// パスが上流で別の場所へ読み替わりうる形 (`..`、`//`、`%2F` 等) をしている (404)。
    UnsafePath,
    /// namespace の allowlist に無い。`method_only` はパスは当たるが method が違う場合 (405)、
    /// それ以外は 404。
    NsAllow { method_only: bool },
    /// 行き先の側の `allow` に無い。`method_only` の意味は同じ。
    UpstreamAllow { method_only: bool },
    /// 固定の秘密が読めない (502)。
    Secret(String),
    /// 上流に届かなかった (502)。
    Unreachable(String),
    /// gateway 自身のバケットが埋まっている (429、DR-0030 §3)。
    RateLimited(gateway_core::ratelimit::Exceeded),
}

impl Refusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::NsAllow { method_only: true } | Self::UpstreamAllow { method_only: true } => 405,
            Self::UnknownUpstream
            | Self::UnsafePath
            | Self::NsAllow { .. }
            | Self::UpstreamAllow { .. } => 404,
            Self::Secret(_) | Self::Unreachable(_) => 502,
            Self::RateLimited(_) => 429,
        }
    }

    /// 知らせ (`refused`) に載せる理由の語。
    pub fn reason(&self) -> &'static str {
        match self {
            Self::UnknownUpstream => "unknown_upstream",
            Self::UnsafePath => "unsafe_path",
            Self::NsAllow { .. } => "ns_allow",
            Self::UpstreamAllow { .. } => "upstream_allow",
            Self::Secret(_) => "secret",
            Self::RateLimited(_) => "rate_limited",
            Self::Unreachable(_) => "unreachable",
        }
    }
}

/// 判定の結果を理由に写す。通すなら `None`。
fn refusal_of(decision: Decision, as_refusal: fn(bool) -> Refusal) -> Option<Refusal> {
    match decision {
        Decision::Allowed => None,
        Decision::UnsafePath => Some(Refusal::UnsafePath),
        Decision::NotFound => Some(as_refusal(false)),
        Decision::MethodNotAllowed => Some(as_refusal(true)),
    }
}

/// 1 本の中継を頼む中身。
pub struct Relay<'a, S> {
    /// ns 認証を通った相手 (検査しない ns では `subject` / `kid` の無い主体)。
    pub principal: gateway_core::ns::Principal,
    /// この namespace の allowlist (`[ns.<name>.allow]`)。
    pub ns_allow: &'a BTreeMap<String, Vec<gateway_core::upstream::Allow>>,
    pub upstream: &'a str,
    pub method: reqwest::Method,
    /// `/` で始まる `<rest>` のパス (クエリを除く)。
    pub path: &'a str,
    /// `?` を除いたクエリ。無ければ `None`。
    pub query: Option<&'a str>,
    pub headers: &'a HeaderMap,
    pub body: S,
}

/// 行き先の一覧と、秘密の読み手。
pub struct Passthrough {
    upstreams: BTreeMap<String, UpstreamSpec>,
    /// 中継専用の HTTP の口。redirect を追わない (下の Design rationale)。
    http: reqwest::Client,
    /// 行き先を 1 つも書いていなければ置き場を開かない (ディレクトリも作らない)。
    secrets: Option<StaticSecretStore<FileStore<StoredSecret>>>,
    /// 秘密の id → その秘密の枠 (`[secrets.<id>].limits`)。
    limits: BTreeMap<String, Vec<gateway_core::ratelimit::Limit>>,
    /// 秘密ごとの分 / 時のバケット。メモリだけで、再起動で消える。
    buckets: gateway_core::ratelimit::MemoryBuckets,
    /// 窓の境界を決める時計。試験で固定するために挟んである。
    clock: gateway_core::credential::refreshing::Clock,
}

/// 上流まで届いた 1 本。
#[derive(Debug)]
pub struct Relayed {
    pub response: reqwest::Response,
    /// gateway 自身のバケットの残量。枠を宣言していない秘密では `None`。
    pub quota: Option<gateway_core::ratelimit::Quota>,
}

impl Passthrough {
    pub fn new(config: &Config) -> Result<Self> {
        let secrets = if config.upstreams.is_empty() {
            None
        } else {
            let files = FileStore::open(config.secret_store.resolve_dir())?;
            Some(StaticSecretStore::new(files))
        };
        // Design rationale: LLM 経路と口を共有しない。共有の口は redirect を
        // 既定どおり追うので、許可した GET に上流が `302 Location: <別 host>` を
        // 返すと、登録した行き先の外へ gateway が出ていく (open proxy)。
        // 3xx は無変換の原則どおりそのままクライアントへ返し、追うかどうかは
        // クライアントが決める。
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Config(format!("could not build the HTTP client: {e}")))?;
        Ok(Self {
            upstreams: config.upstreams.clone(),
            http,
            secrets,
            limits: config
                .secrets
                .iter()
                .filter(|(_, spec)| !spec.limits.is_empty())
                .map(|(id, spec)| (id.clone(), spec.limits.clone()))
                .collect(),
            buckets: gateway_core::ratelimit::MemoryBuckets::new(),
            clock: gateway_core::credential::refreshing::Clock::System,
        })
    }

    /// 窓の境界を決める時計を差し替える (試験用)。
    pub fn set_clock(&mut self, clock: gateway_core::credential::refreshing::Clock) {
        self.clock = clock;
    }

    /// 中継する。返すのは上流の応答そのもの (状態・ヘッダ・本文を変えずに流す)。
    ///
    /// 断った場合も含め、結果は 1 件の知らせとして流す。
    pub async fn relay<S, E>(
        &self,
        events: &Events,
        request: Relay<'_, S>,
    ) -> std::result::Result<Relayed, Refusal>
    where
        S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let started = Instant::now();
        let ts = crate::credential::time::now_unix_ms();
        let (ns, upstream, method, path) = (
            request.principal.ns.clone(),
            request.upstream.to_owned(),
            request.method.as_str().to_owned(),
            request.path.to_owned(),
        );
        let spec = self.upstreams.get(request.upstream);
        let outcome = match spec {
            None => Err(Refusal::UnknownUpstream),
            Some(spec) => self.send(spec, request).await,
        };
        let (status, refused) = match &outcome {
            Ok(relayed) => (relayed.response.status().as_u16(), None),
            Err(refusal) => (refusal.status(), Some(refusal.reason().to_owned())),
        };
        let (bucket, retry_after_secs) = match &outcome {
            Err(Refusal::RateLimited(exceeded)) => (
                Some(exceeded.bucket.clone()),
                Some(exceeded.retry_after_secs),
            ),
            _ => (None, None),
        };
        events.publish(events::Passthrough {
            kind: events::Passthrough::KIND.to_owned(),
            ts,
            seq: 0,
            boot: 0,
            ns,
            upstream,
            method,
            path,
            status,
            duration_ms: started.elapsed().as_millis() as u64,
            secret: spec.map(|s| s.secret.clone()).unwrap_or_default(),
            refused,
            bucket,
            retry_after_secs,
        });
        outcome
    }

    async fn send<S, E>(
        &self,
        spec: &UpstreamSpec,
        request: Relay<'_, S>,
    ) -> std::result::Result<Relayed, Refusal>
    where
        S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        // 判定の順 (計画 rate-limit-and-allowlist §4): namespace の allowlist を
        // 行き先の側より先に見る。ns に見せてよくない行き先の中身 (どのパスが
        // あるか) を、行き先の側の 405 で漏らさないため。
        let method = request.method.as_str();
        let ns_allows = request
            .ns_allow
            .get(request.upstream)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let ns_decision = gateway_core::upstream::decide(ns_allows, method, request.path);
        if let Some(refusal) =
            refusal_of(ns_decision, |method_only| Refusal::NsAllow { method_only })
        {
            return Err(refusal);
        }
        if let Some(refusal) = refusal_of(spec.decide(method, request.path), |method_only| {
            Refusal::UpstreamAllow { method_only }
        }) {
            return Err(refusal);
        }
        let secret = self
            .secrets
            .as_ref()
            .ok_or_else(|| Refusal::Secret("no secret store is open".into()))?
            .get(&CredentialId::new(spec.secret.as_str()))
            .map_err(|e| Refusal::Secret(Error::from(e).to_string()))?;

        let mut headers = forwarded_headers(request.headers, &spec.auth);
        let (name, value) = match &spec.auth {
            AuthPlacement::Bearer => (
                header::AUTHORIZATION,
                format!("Bearer {}", secret.payload.value()),
            ),
            AuthPlacement::Header { header } => (
                HeaderName::from_bytes(header.as_bytes()).map_err(|e| {
                    Refusal::Secret(format!("auth.header is not a header name: {e}"))
                })?,
                secret.payload.value().to_owned(),
            ),
        };
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| Refusal::Secret("the secret cannot be sent as a header value".into()))?;
        value.set_sensitive(true);
        headers.insert(name, value);

        let url = spec
            .target(request.path, request.query)
            .map_err(Refusal::Unreachable)?;
        // 枠は送る直前に数える。秘密が読めずに 502 になる要求や allowlist の外の
        // 要求は上流の枠を減らさないので数えない (計画 rate-limit-and-allowlist §4)。
        // 受理した時点で数え、上流が失敗を返しても 1 と数える。
        let quota = match self.limits.get(&spec.secret) {
            Some(limits) => self
                .buckets
                .take(&spec.secret, limits, self.clock.now_unix())
                .map_err(Refusal::RateLimited)?,
            None => None,
        };
        // 本文は 1 度しか読めないので、流したら取り返せない。断るのはここより前。
        let response = self
            .http
            .request(request.method, url)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(request.body))
            .send()
            .await
            .map_err(|e| Refusal::Unreachable(e.to_string()))?;
        Ok(Relayed { response, quota })
    }
}

/// クライアントのヘッダから、上流へ持っていくもの。
///
/// 落とすのは認証 (`Authorization` と、載せ方に指定されたヘッダ)、`Host`
/// (上流のものを付け直す)、接続ごとの約束だけ。
fn forwarded_headers(from: &HeaderMap, auth: &AuthPlacement) -> HeaderMap {
    let mut headers = without_hop_by_hop(from);
    headers.remove(header::AUTHORIZATION);
    headers.remove(header::HOST);
    if let AuthPlacement::Header { header } = auth {
        headers.remove(header.as_str());
    }
    headers
}

/// 応答のヘッダのうち、クライアントへ持っていくもの (接続ごとの約束を除く)。
pub fn relayed_response_headers(from: &HeaderMap) -> HeaderMap {
    without_hop_by_hop(from)
}

/// 接続ごとの約束を落とす。固定の一覧に加え、`Connection:` で名指しされた
/// ヘッダもその接続限りのもの (RFC 9110 §7.6.1)。
fn without_hop_by_hop(from: &HeaderMap) -> HeaderMap {
    let named: Vec<String> = from
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    let mut headers = from.clone();
    for name in HOP_BY_HOP
        .iter()
        .copied()
        .chain(named.iter().map(String::as_str))
    {
        headers.remove(name);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Connection:` で名指しされたヘッダは往復とも落とす。
    #[test]
    fn headers_named_by_connection_are_dropped_both_ways() {
        let mut from = HeaderMap::new();
        from.insert(
            header::CONNECTION,
            HeaderValue::from_static("x-private, Keep-Alive"),
        );
        from.insert("x-private", HeaderValue::from_static("hop"));
        from.insert("x-kept", HeaderValue::from_static("end-to-end"));

        for headers in [
            forwarded_headers(&from, &AuthPlacement::Bearer),
            relayed_response_headers(&from),
        ] {
            assert!(headers.get("x-private").is_none());
            assert!(headers.get(header::CONNECTION).is_none());
            assert_eq!(headers["x-kept"], "end-to-end");
        }
    }
}
