//! ChatGPT Codex backend の Responses API への出口。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures_util::StreamExt as _;

use crate::egress::{BoxFuture, EgressRequest, Headers, Response, UpstreamRequest};
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
    fn encode(&self, request: EgressRequest) -> Result<UpstreamRequest> {
        let mut headers = request.headers;
        headers.strip_for_upstream();
        headers.extend_from(&self.extra_headers);
        headers.remove("anthropic-version");
        headers.remove("anthropic-beta");
        headers.set("content-type", "application/json");
        headers.set("accept", "text/event-stream");

        let body = Bytes::from(serde_json::to_vec(&request::convert(request.body)?)?);
        Ok(UpstreamRequest {
            url: self.responses_url(),
            headers,
            body,
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
            let is_sse = headers
                .get("content-type")
                .is_some_and(|value| value.split(';').next() == Some("text/event-stream"));
            let body = response
                .bytes_stream()
                .map(|chunk| {
                    chunk.map_err(|error| {
                        Error::Config(format!("Responses 応答の読み取りが途切れました: {error}"))
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
            encoded.url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let body: Value = serde_json::from_slice(&encoded.body).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(encoded.headers.get("accept"), Some("text/event-stream"));
        assert_eq!(encoded.headers.get("anthropic-version"), None);
        assert_eq!(encoded.headers.get("authorization"), None);
    }
}
