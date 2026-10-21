//! 中継の経路が書き手へ渡す記録の形。
//!
//! 中継の経路で組み立てるのは値の束だけで、JSON にするのは書き手のスレッド
//! ([`super::writer`]) の仕事 (reference `model` の「本体を阻害しない」)。

use std::sync::Arc;

use serde_json::{Map, Value, json};

/// `index.jsonl` の 1 行 (reference `files` の表)。
#[derive(Debug, Clone)]
pub(crate) struct Event {
    pub(crate) ts: i64,
    pub(crate) seq: u64,
    pub(crate) kind: Kind,
}

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    RequestHeaders {
        request_file: Arc<str>,
        method: Arc<str>,
        path: Arc<str>,
    },
    RequestBodyEnd {
        bytes: u64,
        duration_ms: i64,
        /// 本文を受け取りきれなかった・読めなかった理由。
        error: Option<&'static str>,
        model: Option<Arc<str>>,
        stream: bool,
    },
    UpstreamConnect {
        attempt: u64,
        route: Arc<str>,
    },
    UpstreamHeaders {
        attempt: u64,
        status: u16,
        response_file: Arc<str>,
        stream: bool,
        duration_ms: i64,
    },
    UpstreamBodyEnd {
        attempt: u64,
        bytes: u64,
        truncated: bool,
        duration_ms: i64,
        error: Option<Arc<str>>,
    },
    UpstreamClose {
        attempt: u64,
        duration_ms: i64,
    },
    ResponseHeadersSent {
        adopted: Option<u64>,
    },
    ResponseBodyEnd {
        bytes: u64,
        completed: bool,
        duration_ms: i64,
    },
    TransportClose {
        duration_ms: i64,
    },
}

impl Event {
    pub(crate) fn to_map(&self) -> Map<String, Value> {
        let mut line = Map::new();
        let mut put = |key: &str, value: Value| {
            line.insert(key.to_owned(), value);
        };
        let name = match &self.kind {
            Kind::RequestHeaders { .. } => "request_headers",
            Kind::RequestBodyEnd { .. } => "request_body_end",
            Kind::UpstreamConnect { .. } => "upstream_connect",
            Kind::UpstreamHeaders { .. } => "upstream_headers",
            Kind::UpstreamBodyEnd { .. } => "upstream_body_end",
            Kind::UpstreamClose { .. } => "upstream_close",
            Kind::ResponseHeadersSent { .. } => "response_headers_sent",
            Kind::ResponseBodyEnd { .. } => "response_body_end",
            Kind::TransportClose { .. } => "transport_close",
        };
        put("event", json!(name));
        put("ts", json!(self.ts));
        put("seq", json!(self.seq));
        match &self.kind {
            Kind::RequestHeaders {
                request_file,
                method,
                path,
            } => {
                put("request_file", json!(&**request_file));
                put("method", json!(&**method));
                put("path", json!(&**path));
            }
            Kind::RequestBodyEnd {
                bytes,
                duration_ms,
                error,
                model,
                stream,
            } => {
                put("bytes", json!(bytes));
                put("duration_ms", json!(duration_ms));
                put("model", json!(model.as_deref()));
                put("stream", json!(stream));
                if let Some(error) = error {
                    put("error", json!(error));
                }
            }
            Kind::UpstreamConnect { attempt, route } => {
                put("attempt", json!(attempt));
                put("route", json!(&**route));
            }
            Kind::UpstreamHeaders {
                attempt,
                status,
                response_file,
                stream,
                duration_ms,
            } => {
                put("attempt", json!(attempt));
                put("status", json!(status));
                put("response_file", json!(&**response_file));
                put("stream", json!(stream));
                put("duration_ms", json!(duration_ms));
            }
            Kind::UpstreamBodyEnd {
                attempt,
                bytes,
                truncated,
                duration_ms,
                error,
            } => {
                put("attempt", json!(attempt));
                put("bytes", json!(bytes));
                put("truncated", json!(truncated));
                put("duration_ms", json!(duration_ms));
                if let Some(error) = error {
                    put("error", json!(&**error));
                }
            }
            Kind::UpstreamClose {
                attempt,
                duration_ms,
            } => {
                put("attempt", json!(attempt));
                put("duration_ms", json!(duration_ms));
            }
            Kind::ResponseHeadersSent { adopted } => put("adopted", json!(adopted)),
            Kind::ResponseBodyEnd {
                bytes,
                completed,
                duration_ms,
            } => {
                put("bytes", json!(bytes));
                put("completed", json!(completed));
                put("duration_ms", json!(duration_ms));
            }
            Kind::TransportClose { duration_ms } => put("duration_ms", json!(duration_ms)),
        }
        line
    }
}

/// `*.request.json` の中身。
#[derive(Debug)]
pub(crate) struct RequestMeta {
    pub(crate) file: Arc<str>,
    pub(crate) seq: u64,
    pub(crate) ts: i64,
    pub(crate) ns: Arc<str>,
    pub(crate) method: Arc<str>,
    pub(crate) path: Arc<str>,
    pub(crate) headers: Arc<[(String, String)]>,
    pub(crate) body_file: Arc<str>,
    pub(crate) body_bytes: u64,
    pub(crate) stream: bool,
    pub(crate) error: Option<&'static str>,
}

impl RequestMeta {
    pub(crate) fn to_json(&self) -> Value {
        let mut meta = json!({
            "seq": self.seq,
            "ts": self.ts,
            "ns": &*self.ns,
            "method": &*self.method,
            "path": &*self.path,
            "headers": kept_headers(&self.headers),
            "content_type": header(&self.headers, "content-type"),
            "content_encoding": header(&self.headers, "content-encoding"),
            "stream": self.stream,
            "body_file": &*self.body_file,
            "body_stored": "raw",
            "body_bytes": self.body_bytes,
        });
        if let Some(error) = self.error {
            meta["error"] = json!(error);
        }
        meta
    }
}

/// `*.<attempt>.response.json` の中身。
#[derive(Debug)]
pub(crate) struct ResponseMeta {
    pub(crate) file: Arc<str>,
    pub(crate) seq: u64,
    pub(crate) ts: i64,
    pub(crate) attempt: u64,
    pub(crate) route: Arc<str>,
    pub(crate) credential: Option<Arc<str>>,
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body_file: Arc<str>,
    pub(crate) stream: bool,
    pub(crate) request_rewrites: Map<String, Value>,
}

impl ResponseMeta {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "ts": self.ts,
            "attempt": self.attempt,
            "route": &*self.route,
            "credential": self.credential.as_deref(),
            "status": self.status,
            "headers": kept_headers(&self.headers),
            "content_type": header(&self.headers, "content-type"),
            "content_encoding": header(&self.headers, "content-encoding"),
            "stream": self.stream,
            "body_file": &*self.body_file,
            "body_stored": "raw",
            "request_rewrites": self.request_rewrites,
        })
    }
}

pub(crate) fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// メタ json に書くヘッダ: 受けた順の `{name, value}` の配列。同名を繋がない。
/// 認証系 (keepalive の控えと同じ述語) だけを落とす。
fn kept_headers(headers: &[(String, String)]) -> Vec<Value> {
    headers
        .iter()
        .filter(|(name, _)| !crate::gateway::is_authorization(name))
        .map(|(name, value)| json!({"name": name.to_ascii_lowercase(), "value": value}))
        .collect()
}
