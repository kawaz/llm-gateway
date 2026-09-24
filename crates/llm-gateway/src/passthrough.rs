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
    /// 許可に当たるパスが無い (404)。
    NotAllowed,
    /// パスは許可にあるが method が違う (405)。
    MethodNotAllowed,
    /// 固定の秘密が読めない (502)。
    Secret(String),
    /// 上流に届かなかった (502)。
    Unreachable(String),
}

impl Refusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::UnknownUpstream | Self::NotAllowed => 404,
            Self::MethodNotAllowed => 405,
            Self::Secret(_) | Self::Unreachable(_) => 502,
        }
    }
}

/// 1 本の中継を頼む中身。
pub struct Relay<'a, S> {
    pub ns: &'a str,
    pub upstream: &'a str,
    pub method: reqwest::Method,
    /// `/` で始まる `<rest>` のパス (クエリを除く)。
    pub path: &'a str,
    /// `?` から後ろ。無ければ空。
    pub query: &'a str,
    pub headers: &'a HeaderMap,
    pub body: S,
}

/// 行き先の一覧と、秘密の読み手。
pub struct Passthrough {
    upstreams: BTreeMap<String, UpstreamSpec>,
    /// 行き先を 1 つも書いていなければ置き場を開かない (ディレクトリも作らない)。
    secrets: Option<StaticSecretStore<FileStore<StoredSecret>>>,
}

impl Passthrough {
    pub fn new(config: &Config) -> Result<Self> {
        let secrets = if config.upstreams.is_empty() {
            None
        } else {
            let files = FileStore::open(config.secrets.resolve_dir())?;
            Some(StaticSecretStore::new(files))
        };
        Ok(Self {
            upstreams: config.upstreams.clone(),
            secrets,
        })
    }

    /// 中継する。返すのは上流の応答そのもの (状態・ヘッダ・本文を変えずに流す)。
    ///
    /// 断った場合も含め、結果は 1 件の知らせとして流す。
    pub async fn relay<S, E>(
        &self,
        http: &reqwest::Client,
        events: &Events,
        request: Relay<'_, S>,
    ) -> std::result::Result<reqwest::Response, Refusal>
    where
        S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let started = Instant::now();
        let ts = crate::credential::time::now_unix_ms();
        let (ns, upstream, method, path) = (
            request.ns.to_owned(),
            request.upstream.to_owned(),
            request.method.as_str().to_owned(),
            request.path.to_owned(),
        );
        let spec = self.upstreams.get(request.upstream);
        let outcome = match spec {
            None => Err(Refusal::UnknownUpstream),
            Some(spec) => self.send(http, spec, request).await,
        };
        let status = match &outcome {
            Ok(resp) => resp.status().as_u16(),
            Err(refusal) => refusal.status(),
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
        });
        outcome
    }

    async fn send<S, E>(
        &self,
        http: &reqwest::Client,
        spec: &UpstreamSpec,
        request: Relay<'_, S>,
    ) -> std::result::Result<reqwest::Response, Refusal>
    where
        S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        match spec.decide(request.method.as_str(), request.path) {
            Decision::Allowed => {}
            Decision::NotFound => return Err(Refusal::NotAllowed),
            Decision::MethodNotAllowed => return Err(Refusal::MethodNotAllowed),
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

        let url = format!("{}{}", spec.target(request.path), request.query);
        // 本文は 1 度しか読めないので、流したら取り返せない。断るのはここより前。
        http.request(request.method, url)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(request.body))
            .send()
            .await
            .map_err(|e| Refusal::Unreachable(e.to_string()))
    }
}

/// クライアントのヘッダから、上流へ持っていくもの。
///
/// 落とすのは認証 (`Authorization` と、載せ方に指定されたヘッダ)、`Host`
/// (上流のものを付け直す)、接続ごとの約束だけ。
fn forwarded_headers(from: &HeaderMap, auth: &AuthPlacement) -> HeaderMap {
    let mut headers = from.clone();
    headers.remove(header::AUTHORIZATION);
    headers.remove(header::HOST);
    if let AuthPlacement::Header { header } = auth {
        headers.remove(header.as_str());
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    headers
}

/// 応答のヘッダのうち、クライアントへ持っていくもの (接続ごとの約束を除く)。
pub fn relayed_response_headers(from: &HeaderMap) -> HeaderMap {
    let mut headers = from.clone();
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    headers
}
