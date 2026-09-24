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
    /// パスが上流で別の場所へ読み替わりうる形 (`..`、`//`、`%2F` 等) をしている (404)。
    UnsafePath,
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
            Self::UnknownUpstream | Self::NotAllowed | Self::UnsafePath => 404,
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
}

impl Passthrough {
    pub fn new(config: &Config) -> Result<Self> {
        let secrets = if config.upstreams.is_empty() {
            None
        } else {
            let files = FileStore::open(config.secrets.resolve_dir())?;
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
        })
    }

    /// 中継する。返すのは上流の応答そのもの (状態・ヘッダ・本文を変えずに流す)。
    ///
    /// 断った場合も含め、結果は 1 件の知らせとして流す。
    pub async fn relay<S, E>(
        &self,
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
            Some(spec) => self.send(spec, request).await,
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
            Decision::UnsafePath => return Err(Refusal::UnsafePath),
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
        // 本文は 1 度しか読めないので、流したら取り返せない。断るのはここより前。
        self.http
            .request(request.method, url)
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
