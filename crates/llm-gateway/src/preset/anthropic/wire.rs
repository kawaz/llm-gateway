//! Anthropic Messages API への変換と送信。
//!
//! 正規形が既に Messages 形式なので、変換は接続先と付随ヘッダを合わせる
//! だけ。ボディは [`Wire::encode`] で **1 度だけ**直列化し、その [`Bytes`] を
//! 認証と送信が共有する — 認証の後に直列化し直すと、署名した中身と送る中身が
//! ずれる余地ができる。
//!
//! **モデル名の読み替えはここに無い**。どの名前を upstream が求めるかは
//! discovery が答えるので、送る本文に載せるのは呼び出し側の仕事
//! ([`crate::egress::rewrite_model`])。方言の話ではないものを方言の実装に
//! 持たせると、同じ Wire を使う preset ごとに設定を配り直すことになる。
//!
//! 応答はヘッダを受け取った時点で返し、本文はストリームのまま流す。Claude 系は
//! SSE の形式が upstream 間で同じなので、解釈せずバイト列のまま中継できる。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures_util::StreamExt as _;

use crate::egress::{
    BoxFuture, EgressRequest, EncodedRequest, Headers, Response, ResponseMode, UpstreamRequest,
};
use crate::provider::Wire;
use crate::{Error, Result};

/// Messages API への出口。
///
/// upstream ごとに違うのは接続先と付随ヘッダだけなので、実装ではなく**設定**
/// として持つ。Bedrock はこの型を書き直さず、値を差し替えて使う (DR-0014 §6)。
pub struct AnthropicWire {
    /// 失敗の記録に出す経路の名前。
    name: String,
    /// 転送先の根 (`/v1/messages` などを足す前)。
    base_url: String,
    /// 設定で足すヘッダ。
    extra_headers: BTreeMap<String, String>,
}

impl AnthropicWire {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            extra_headers,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url_for(&self, path: &str, query: Option<&str>) -> String {
        let mut url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        if let Some(q) = query.filter(|q| !q.is_empty()) {
            url.push('?');
            url.push_str(q);
        }
        url
    }
}

impl Wire for AnthropicWire {
    fn encode(&self, request: EgressRequest) -> Result<EncodedRequest> {
        let EgressRequest {
            path,
            query,
            body,
            mut headers,
        } = request;

        headers.strip_for_upstream();
        headers.extend_from(&self.extra_headers);

        let body = Bytes::from(serde_json::to_vec(&body)?);
        // 直列化した後に載せる。クライアントの申告ではなく、実際に送る形。
        headers.set("content-type", "application/json");

        Ok(EncodedRequest {
            upstream: UpstreamRequest {
                url: self.url_for(&path, query.as_deref()),
                headers,
                body,
            },
            response: ResponseMode::Passthrough,
        })
    }

    fn send<'a>(
        &'a self,
        http: &'a reqwest::Client,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async move {
            let UpstreamRequest { url, headers, body } = request;

            let mut req = http.post(&url).body(body);
            for (key, value) in headers.iter() {
                req = req.header(key, value);
            }

            let resp = req.send().await.map_err(|e| Error::UpstreamUnreachable {
                provider: self.name.clone(),
                source: e,
            })?;

            let status = resp.status().as_u16();
            let headers = Headers::new(
                resp.headers()
                    .iter()
                    .filter(|(key, _)| !is_hop_by_hop(key.as_str()))
                    .filter_map(|(key, value)| {
                        value
                            .to_str()
                            .ok()
                            .map(|value| (key.as_str().to_owned(), value.to_owned()))
                    })
                    .collect(),
            );

            let body = resp
                .bytes_stream()
                .map(|chunk| {
                    chunk.map_err(|e| {
                        Error::Config(format!("response reading was interrupted: {e}"))
                    })
                })
                .boxed();

            Ok(Response {
                status,
                headers,
                body,
            })
        })
    }
}

/// この応答ヘッダは中継の途中で意味を失うか。
fn is_hop_by_hop(name: &str) -> bool {
    const DROP: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        // 中継で長さが変わりうるので、こちらで付け直す。
        "content-length",
    ];
    DROP.iter().any(|d| name.eq_ignore_ascii_case(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Claude Code 2.1.220 が実際に送る beta の束。
    const CLIENT_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
thinking-token-count-2026-05-13,context-management-2025-06-27,\
prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
extended-cache-ttl-2025-04-11";

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        Headers::new(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    fn request(body: Value, headers: Headers) -> EgressRequest {
        EgressRequest {
            path: "/v1/messages".to_owned(),
            query: None,
            body,
            headers,
        }
    }

    fn official() -> AnthropicWire {
        AnthropicWire::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
        )
    }

    /// ボディは中身も並びも変えずに送る。モデル名の読み替えは呼び出し側。
    #[test]
    fn passes_the_body_through() {
        let body = json!({"model": "claude-opus-5", "max_tokens": 8});
        let encoded = official()
            .encode(request(body.clone(), headers(&[])))
            .unwrap();

        assert_eq!(
            serde_json::from_slice::<Value>(&encoded.upstream.body).unwrap(),
            body
        );
        assert_eq!(encoded.response, ResponseMode::Passthrough);
    }

    /// 実クライアントが送るヘッダを通したとき、何が残り何が消えるか。
    #[test]
    fn strips_connection_and_auth_but_keeps_the_rest() {
        let encoded = official()
            .encode(request(
                json!({"model": "claude-opus-5"}),
                headers(&[
                    ("Host", "127.0.0.1:11300"),
                    ("Connection", "keep-alive"),
                    ("Content-Length", "253966"),
                    ("Accept-Encoding", "gzip, deflate, br, zstd"),
                    ("Authorization", "Bearer client-token"),
                    ("anthropic-version", "2023-06-01"),
                    ("anthropic-beta", CLIENT_BETA),
                    ("X-Claude-Code-Session-Id", "8f17d3dd"),
                    ("User-Agent", "claude-cli/2.1.220"),
                    ("x-app", "cli"),
                ]),
            ))
            .unwrap();

        let names: Vec<&str> = encoded
            .upstream
            .headers
            .iter()
            .map(|(key, _)| key)
            .collect();
        for gone in [
            "Host",
            "Connection",
            "Content-Length",
            "Accept-Encoding",
            "Authorization",
        ] {
            assert!(!names.contains(&gone), "{gone} should be dropped: {names:?}");
        }
        for kept in [
            "anthropic-version",
            "anthropic-beta",
            "X-Claude-Code-Session-Id",
            "User-Agent",
            "x-app",
        ] {
            assert!(names.contains(&kept), "{kept} should be kept: {names:?}");
        }
        assert_eq!(
            encoded.upstream.headers.get("anthropic-beta"),
            Some(CLIENT_BETA),
            "beta selection is negotiation's job (DR-0003); wire leaves it alone"
        );
    }

    /// 手前のプロキシが付けた経路情報は upstream へ渡さない。
    ///
    /// 前段に Caddy を置くと、クライアントの LAN / tailnet の IP や内向きの
    /// ホスト名が乗ってくる。upstream から見えるのはこの gateway なので、
    /// こちら側の内部経路を知らせる理由がない。
    #[test]
    fn strips_proxy_added_route_information() {
        let encoded = official()
            .encode(request(
                json!({"model": "m"}),
                headers(&[
                    ("X-Forwarded-For", "100.76.5.89"),
                    ("X-Forwarded-Proto", "https"),
                    ("X-Forwarded-Host", "llm-gateway.example.internal"),
                    ("X-Forwarded-Port", "443"),
                    ("X-Real-IP", "100.76.5.89"),
                    ("Forwarded", "for=100.76.5.89;proto=https"),
                    ("Via", "1.1 caddy"),
                    ("anthropic-version", "2023-06-01"),
                ]),
            ))
            .unwrap();

        let names: Vec<&str> = encoded
            .upstream
            .headers
            .iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            names,
            vec!["anthropic-version", "content-type"],
            "drops only route information and leaves the rest alone: {names:?}"
        );
    }

    /// 認証は表記が違っても落とす (client が x-api-key を送ってきた場合)。
    ///
    /// 落とさないと、クライアントの鍵が upstream の認証と二重に載る。
    #[test]
    fn strips_client_auth_regardless_of_case() {
        let encoded = official()
            .encode(request(
                json!({"model": "m"}),
                headers(&[("X-API-KEY", "client-key"), ("AUTHORIZATION", "Bearer x")]),
            ))
            .unwrap();

        assert_eq!(encoded.upstream.headers.get("x-api-key"), None);
        assert_eq!(encoded.upstream.headers.get("authorization"), None);
    }

    /// 設定で足したヘッダが載る。
    #[test]
    fn extra_headers_from_config_are_applied() {
        let wire = AnthropicWire::new(
            "c",
            "https://api.anthropic.com",
            BTreeMap::from([("x-trace".to_owned(), "on".to_owned())]),
        );
        let encoded = wire
            .encode(request(
                json!({"model": "m"}),
                headers(&[("anthropic-version", "2023-06-01")]),
            ))
            .unwrap();

        assert_eq!(encoded.upstream.headers.get("x-trace"), Some("on"));
        assert_eq!(
            encoded.upstream.headers.get("anthropic-version"),
            Some("2023-06-01")
        );
    }

    /// 送る中身が JSON であることは、こちらが決めて申告する。
    #[test]
    fn declares_the_content_type_it_actually_sends() {
        let encoded = official()
            .encode(request(
                json!({"model": "m"}),
                headers(&[("content-type", "application/json; charset=utf-8")]),
            ))
            .unwrap();

        assert_eq!(
            encoded.upstream.headers.get("Content-Type"),
            Some("application/json")
        );
        assert_eq!(
            encoded
                .upstream
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-type"))
                .count(),
            1,
            "not added twice"
        );
    }

    #[test]
    fn builds_the_url_from_base_and_path() {
        assert_eq!(
            official().url_for("/v1/messages", None),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            official().url_for("/v1/messages", Some("beta=true")),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(
            official().url_for("/v1/messages", Some("")),
            "https://api.anthropic.com/v1/messages",
            "an empty query is not appended"
        );
    }

    /// 末尾の `/` が二重にならない。
    #[test]
    fn trims_the_trailing_slash_of_the_base_url() {
        let wire = AnthropicWire::new("c", "http://127.0.0.1:8317/", BTreeMap::new());
        assert_eq!(
            wire.url_for("/v1/messages", None),
            "http://127.0.0.1:8317/v1/messages"
        );
    }

    #[test]
    fn hop_by_hop_detection_ignores_case() {
        assert!(is_hop_by_hop("Transfer-Encoding"));
        assert!(is_hop_by_hop("connection"));
        assert!(is_hop_by_hop("CONTENT-LENGTH"));
        assert!(!is_hop_by_hop("content-type"));
        assert!(!is_hop_by_hop("anthropic-version"));
    }
}
