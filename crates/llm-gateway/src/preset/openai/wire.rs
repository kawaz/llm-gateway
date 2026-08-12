//! ChatGPT Codex backend の Responses API への出口。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures_util::StreamExt as _;
use serde_json::Value;

use crate::egress::{
    BoxFuture, EgressRequest, EncodedRequest, Headers, Response, ResponseMode, UpstreamRequest,
};
use crate::provider::Wire;
use crate::{Error, Result};

use super::{request, response};

pub struct OpenAiWire {
    name: String,
    base_url: String,
    extra_headers: BTreeMap<String, String>,
}

impl OpenAiWire {
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

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }
}

impl Wire for OpenAiWire {
    fn encode(&self, request: EgressRequest) -> Result<EncodedRequest> {
        let client_streams = request.body.get("stream").and_then(Value::as_bool) == Some(true);
        let mut headers = request.headers;
        headers.strip_for_upstream();
        headers.extend_from(&self.extra_headers);
        headers.remove("anthropic-version");
        headers.remove("anthropic-beta");
        headers.set("content-type", "application/json");
        headers.set("accept", "text/event-stream");

        let body = Bytes::from(serde_json::to_vec(&request::convert(request.body)?)?);
        Ok(EncodedRequest {
            upstream: UpstreamRequest {
                url: self.responses_url(),
                headers,
                body,
            },
            response: if client_streams {
                ResponseMode::Passthrough
            } else {
                ResponseMode::CollectMessagesSse
            },
        })
    }

    fn send<'a>(
        &'a self,
        http: &'a reqwest::Client,
        request: UpstreamRequest,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async move {
            let UpstreamRequest { url, headers, body } = request;
            let mut sending = http.post(&url).body(body);
            for (key, value) in headers.iter() {
                sending = sending.header(key, value);
            }
            let response = sending
                .send()
                .await
                .map_err(|source| Error::UpstreamUnreachable {
                    provider: self.name.clone(),
                    source,
                })?;
            let status = response.status().as_u16();
            let headers = Headers::new(
                response
                    .headers()
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
            let is_sse = is_event_stream(status, headers.get("content-type"));
            let body = response
                .bytes_stream()
                .map(|chunk| {
                    chunk.map_err(|error| {
                        Error::Config(format!(
                            "Responses API response reading was interrupted: {error}"
                        ))
                    })
                })
                .boxed();
            let body = if is_sse {
                response::translate(body)
            } else {
                body
            };
            let mut headers = headers;
            if is_sse {
                headers.set("content-type", "text/event-stream");
            }
            Ok(Response {
                status,
                headers,
                body,
            })
        })
    }
}

/// 応答が SSE かどうか。
///
/// この Wire は必ず `stream=true` で送るので、成功応答は SSE しかありえない。
/// content-type だけを見ると判定を外す: Codex backend は SSE を返しながら
/// content-type を付けない (実測 2026-08-11)。名乗らない成功応答は SSE と見なす。
/// 失敗応答は JSON なので、そのまま素通しして Metering に読ませる。
fn is_event_stream(status: u16, content_type: Option<&str>) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    match content_type {
        None => true,
        Some(value) => value.split(';').next() == Some("text/event-stream"),
    }
}

fn is_hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ]
    .iter()
    .any(|drop| name.eq_ignore_ascii_case(drop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Messages endpoint の path を受けても Codex backend の `/responses` へ変換する。
    #[test]
    fn encodes_a_responses_request() {
        let wire = OpenAiWire::new(
            "codex",
            "https://chatgpt.com/backend-api/codex",
            BTreeMap::new(),
        );
        let encoded = wire
            .encode(EgressRequest {
                path: "/v1/messages".to_owned(),
                query: None,
                body: json!({"model":"gpt-5.3-codex","max_tokens":1024,"messages":[]}),
                headers: Headers::new(vec![
                    ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
                    ("authorization".to_owned(), "Bearer client".to_owned()),
                ]),
            })
            .unwrap();

        assert_eq!(
            encoded.upstream.url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let body: Value = serde_json::from_slice(&encoded.upstream.body).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(
            encoded.upstream.headers.get("accept"),
            Some("text/event-stream")
        );
        assert_eq!(encoded.upstream.headers.get("anthropic-version"), None);
        assert_eq!(encoded.upstream.headers.get("authorization"), None);
        assert_eq!(encoded.response, ResponseMode::CollectMessagesSse);
    }

    /// upstream は常に SSE だが、client が明示的に `stream:true` を要求した場合だけ
    /// SSE のまま返す。未指定と false は Anthropic の既定どおり単一 JSON にする。
    #[test]
    fn chooses_the_client_response_shape_from_stream() {
        let wire = OpenAiWire::new("codex", "https://example.invalid", BTreeMap::new());
        for (stream, expected) in [
            (None, ResponseMode::CollectMessagesSse),
            (Some(false), ResponseMode::CollectMessagesSse),
            (Some(true), ResponseMode::Passthrough),
        ] {
            let mut body = json!({"model":"m","max_tokens":8,"messages":[]});
            if let Some(stream) = stream {
                body["stream"] = Value::Bool(stream);
            }
            let encoded = wire
                .encode(EgressRequest {
                    path: "/v1/messages".to_owned(),
                    query: None,
                    body,
                    headers: Headers::default(),
                })
                .unwrap();

            assert_eq!(encoded.response, expected, "stream={stream:?}");
            let upstream: Value = serde_json::from_slice(&encoded.upstream.body).unwrap();
            assert_eq!(upstream["stream"], true, "upstream は常に SSE");
        }
    }

    /// content-type を名乗らない成功応答も SSE として変換する。
    /// Codex backend は SSE を返しながら content-type を付けない。
    #[test]
    fn a_success_without_content_type_is_still_translated() {
        assert!(is_event_stream(200, None));
        assert!(is_event_stream(200, Some("text/event-stream")));
        assert!(is_event_stream(
            200,
            Some("text/event-stream; charset=utf-8")
        ));

        // 失敗応答は JSON。素通しして Metering に読ませる。
        assert!(!is_event_stream(429, None));
        assert!(!is_event_stream(400, Some("application/json")));
        // 成功でも別の型を名乗るなら変換しない。
        assert!(!is_event_stream(200, Some("application/json")));
    }
}
