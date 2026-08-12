//! upstream へ出るときの provider-neutral な HTTP 契約と、出口の手順。
//!
//! ingress が作った Messages 形式の正規形を [`EgressRequest`] で受け、provider の
//! [`crate::provider::Wire`] が送信可能な [`UpstreamRequest`] へ変換する。
//! 署名と実送信は同じ [`bytes::Bytes`] を参照し、JSON の再直列化で内容が変わらない。
//!
//! 手順もここが持つ。1 本送る ([`send`])、返ってきた本文を読む
//! ([`collect_body`] / [`buffer`]) の 3 つは、どの preset を選んでも同じ形に
//! なる出口の作法で、provider ごとに違うのはその中で呼ばれる Wire と Auth
//! だけになる。
//!
//! 正規形の `model` 欄を読み書きする [`model_of`] / [`rewrite_model`] も
//! ここに置く。どの名前で呼ぶかは方言ではなく正規形の話 (DR-0014 §5) なので、
//! 方言の実装に持たせると同じものを preset の数だけ書くことになる。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use serde_json::Value;

use crate::credential::Credential;
use crate::provider::Preset;
use crate::{Error, Result};

/// object-safe な非同期 trait method の戻り値。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 応答本文のストリーム。
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// ヘッダの入れ物。
///
/// 名前の大小を無視して引ける必要がある一方、upstream へは受け取った表記で
/// 送りたい。複数 provider の Wire/Auth が同じ操作を使うため core に置く。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 同じ名前があれば置き換え、無ければ足す。
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        match self
            .0
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            Some((_, current)) => *current = value.into(),
            None => self.0.push((name.to_owned(), value.into())),
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.0.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// 受け取った順の組をそのまま見る。
    ///
    /// 名前で引く道具を持たない読み手 (ヘッダを総なめする parser) へ渡すため。
    pub fn as_slice(&self) -> &[(String, String)] {
        &self.0
    }

    pub fn extend_from(&mut self, extra: &BTreeMap<String, String>) {
        for (key, value) in extra {
            self.set(key, value.clone());
        }
    }

    /// クライアント由来のヘッダのうち、upstream へ渡さないものを落とす。
    pub fn strip_for_upstream(&mut self) {
        const DROP: &[&str] = &[
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
            "authorization",
            "x-api-key",
            "accept-encoding",
            "x-forwarded-for",
            "x-forwarded-proto",
            "x-forwarded-host",
            "x-forwarded-port",
            "x-real-ip",
            "forwarded",
            "via",
        ];
        self.0
            .retain(|(key, _)| !DROP.iter().any(|drop| key.eq_ignore_ascii_case(drop)));
    }
}

/// ingress から egress へ渡す正規形。
///
/// `body` は Messages 形式の `serde_json::Value`。中立 IR は挟まない。
#[derive(Debug, Clone)]
pub struct EgressRequest {
    pub path: String,
    pub query: Option<String>,
    pub body: Value,
    pub headers: Headers,
}

/// Auth が認証し、Wire が送る HTTP リクエスト。
///
/// `body` は Wire が一度だけ直列化した値。署名する Auth と送信処理が同じ bytes を
/// 使うことで、署名後の再直列化による差を作らない。
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub url: String,
    pub headers: Headers,
    pub body: Bytes,
}

/// Wire が組み立てた送信内容と、正規形でクライアントへ返す方法。
#[derive(Debug, Clone)]
pub struct EncodedRequest {
    pub upstream: UpstreamRequest,
    pub response: ResponseMode,
}

/// upstream 応答をクライアント向けの正規形へ整える方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    /// Wire が返した本文と content-type をそのまま流す。
    Passthrough,
    /// 正規形の Messages SSE を単一の Messages JSON に集約する。
    CollectMessagesSse,
}

/// upstream から受けた応答と、採用後に適用するクライアント向けの仕上げ。
pub struct SentResponse {
    pub response: Response,
    pub mode: ResponseMode,
}

/// upstream からの応答。本文はまだ読んでいない。
pub struct Response {
    pub status: u16,
    pub headers: Headers,
    pub body: BodyStream,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &"<stream>")
            .finish()
    }
}

/// 1 本送る。
///
/// 返るのは upstream のヘッダを受け取った時点。本文はまだ流れていないので、
/// ここまでの失敗なら別の upstream に切り替えられる。
///
/// 順は **組み立て → 認証 → 送信**。認証を最後にするのは、載せるものが
/// 直列化済みのリクエスト全体に掛かりうるため (SigV4 のような署名は、後から
/// ヘッダやボディが変わると壊れる)。
pub async fn send(
    http: &reqwest::Client,
    preset: &Preset,
    credential: Option<&Credential>,
    request: EgressRequest,
) -> Result<SentResponse> {
    let EncodedRequest {
        mut upstream,
        response: mode,
    } = preset.wire().encode(request)?;
    preset.auth().authorize(credential, &mut upstream)?;
    let response = preset.wire().send(http, upstream).await?;
    Ok(SentResponse { response, mode })
}

pub async fn finish_response(sent: SentResponse) -> Result<Response> {
    adapt_response(sent.response, sent.mode).await
}

async fn adapt_response(mut response: Response, mode: ResponseMode) -> Result<Response> {
    if mode == ResponseMode::Passthrough
        || !response
            .headers
            .get("content-type")
            .is_some_and(is_event_stream)
    {
        return Ok(response);
    }

    let raw = collect_body(response.body).await?;
    let body = collect_messages_sse(&raw)?;
    response.headers.set("content-type", "application/json");
    response.body = futures_util::stream::once(async move { Ok(Bytes::from(body)) }).boxed();
    Ok(response)
}

fn is_event_stream(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn collect_messages_sse(raw: &[u8]) -> Result<Vec<u8>> {
    let mut collector = MessageCollector::default();
    let mut data = Vec::new();

    for line in raw.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if !data.is_empty() {
                collector.event(&data)?;
                data.clear();
            }
            continue;
        }
        let Some(value) = line.strip_prefix(b"data:") else {
            continue;
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(value);
    }
    if !data.is_empty() {
        collector.event(&data)?;
    }

    serde_json::to_vec(&collector.finish()).map_err(Into::into)
}

#[derive(Default)]
struct MessageCollector {
    message: Option<Value>,
    tool_input: BTreeMap<usize, String>,
    stopped: bool,
    error: Option<Value>,
}

impl MessageCollector {
    fn event(&mut self, raw: &[u8]) -> Result<()> {
        let event: Value = serde_json::from_slice(raw)?;
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => self.start_message(&event)?,
            "content_block_start" => self.start_block(&event),
            "content_block_delta" => self.apply_delta(&event)?,
            "content_block_stop" => self.stop_block(&event)?,
            "message_delta" => self.apply_message_delta(&event),
            "message_stop" => self.stopped = true,
            "error" => self.error = Some(event),
            _ => {}
        }
        Ok(())
    }

    fn start_message(&mut self, event: &Value) -> Result<()> {
        let message = event
            .get("message")
            .ok_or_else(|| Error::Config("Messages SSE message_start has no message".to_owned()))?;
        let content = message.get("content").and_then(Value::as_array);
        if !message.is_object() || content.is_none() {
            return Err(Error::Config(
                "Messages SSE message_start message has an unexpected shape".to_owned(),
            ));
        }
        self.message = Some(message.clone());
        Ok(())
    }

    fn start_block(&mut self, event: &Value) {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return;
        };
        let Some(block) = event.get("content_block").cloned() else {
            return;
        };
        let index = index as usize;
        let content = self.content_mut();
        if content.len() <= index {
            content.resize(index + 1, Value::Null);
        }
        content[index] = block;
    }

    fn apply_delta(&mut self, event: &Value) -> Result<()> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Config("Messages SSE delta has no index".to_owned()))?
            as usize;
        let delta = event.get("delta").unwrap_or(&Value::Null);
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => {
                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                let block = self
                    .content_mut()
                    .get_mut(index)
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| {
                        Error::Config("Messages SSE text delta has no block".to_owned())
                    })?;
                let current = block
                    .entry("text")
                    .or_insert_with(|| Value::String(String::new()));
                let Value::String(current) = current else {
                    return Err(Error::Config(
                        "Messages SSE text block's text is not a string".to_owned(),
                    ));
                };
                current.push_str(text);
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.tool_input.entry(index).or_default().push_str(partial);
            }
            _ => {}
        }
        Ok(())
    }

    fn stop_block(&mut self, event: &Value) -> Result<()> {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return Ok(());
        };
        let index = index as usize;
        let Some(raw) = self.tool_input.remove(&index) else {
            return Ok(());
        };
        let input: Value = serde_json::from_str(&raw)?;
        let block = self
            .content_mut()
            .get_mut(index)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| Error::Config("Messages SSE tool delta has no block".to_owned()))?;
        block.insert("input".to_owned(), input);
        Ok(())
    }

    fn apply_message_delta(&mut self, event: &Value) {
        let message = self.message_mut();
        if let Some(delta) = event.get("delta").and_then(Value::as_object) {
            for key in ["stop_reason", "stop_sequence"] {
                if let Some(value) = delta.get(key) {
                    message.insert(key.to_owned(), value.clone());
                }
            }
        }
        if let Some(usage) = event.get("usage") {
            message.insert("usage".to_owned(), usage.clone());
        }
    }

    fn content_mut(&mut self) -> &mut Vec<Value> {
        self.message_mut()
            .entry("content")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("message content は配列として初期化する")
    }

    fn message_mut(&mut self) -> &mut serde_json::Map<String, Value> {
        self.message
            .get_or_insert_with(|| {
                serde_json::json!({
                    "id":"msg_unknown",
                    "type":"message",
                    "role":"assistant",
                    "model":"",
                    "content":[],
                    "stop_reason":null,
                    "stop_sequence":null,
                    "usage":{"input_tokens":0,"output_tokens":0}
                })
            })
            .as_object_mut()
            .expect("message は object として初期化する")
    }

    fn finish(self) -> Value {
        if let Some(error) = self.error {
            return error;
        }
        if !self.stopped {
            return serde_json::json!({
                "type":"error",
                "error":{
                    "type":"api_error",
                    "message":"upstream stream ended before message_stop"
                }
            });
        }
        self.message.unwrap_or_else(|| {
            serde_json::json!({
                "type":"error",
                "error":{
                    "type":"api_error",
                    "message":"upstream stream ended without message_start"
                }
            })
        })
    }
}

/// 応答をすべて読む。
///
/// 失敗時の本文を見たいときや、ストリームでない応答を扱うときに使う。
pub async fn collect_body(body: BodyStream) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut body = body;
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// 本文を読んだうえで、まだ読んでいない応答として返し直す。
///
/// 失敗の中身を見てから「そのままクライアントへ返す」ことができる。
/// 読んだら返せない作りだと、中身を見るために応答を捨てる羽目になる。
/// 呼ぶのは失敗時だけ — 生成中の応答を丸ごと抱えると、その分の遅延と
/// メモリがそのまま乗る。
pub async fn buffer(resp: Response) -> Result<(Response, Vec<u8>)> {
    let Response {
        status,
        headers,
        body,
    } = resp;
    let raw = collect_body(body).await?;
    let replayed = futures_util::stream::once({
        let raw = raw.clone();
        async move { Ok(Bytes::from(raw)) }
    })
    .boxed();

    Ok((
        Response {
            status,
            headers,
            body: replayed,
        },
        raw,
    ))
}

/// 正規形からモデル名を読む。
pub fn model_of(body: &Value) -> Result<&str> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Config("request has no model".to_owned()))
}

/// 正規形の `model` を、upstream が求める名前に替える。
///
/// どの名前を求めるかは discovery が答える (独自の名前空間を付ける upstream が
/// ある)。正規形が Messages 形式なので、書き換えは 1 欄で済む。
pub fn rewrite_model(body: &mut Value, upstream_name: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_owned(), Value::String(upstream_name.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> Headers {
        Headers::new(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    /// Auth と Wire が表記の違う同じヘッダを二重に載せない。
    #[test]
    fn header_updates_ignore_case() {
        let mut headers = headers(&[("Authorization", "old")]);
        headers.set("authorization", "new");

        assert_eq!(headers.iter().count(), 1);
        assert_eq!(headers.get("AUTHORIZATION"), Some("new"));
    }

    /// クライアント側の認証・接続・内部経路情報は upstream へ渡さず、API の
    /// バージョンやクライアント情報は残す。
    #[test]
    fn strips_transport_auth_and_proxy_headers() {
        let mut headers = headers(&[
            ("Host", "gateway.invalid"),
            ("Authorization", "Bearer client"),
            ("X-Forwarded-For", "192.0.2.1"),
            ("x-api-version", "2026-01-01"),
            ("user-agent", "client/1"),
        ]);
        headers.strip_for_upstream();

        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("x-api-version", "2026-01-01"), ("user-agent", "client/1")]
        );
    }

    /// 署名対象と送信対象は同じ Bytes 値を clone して共有できる。
    #[test]
    fn upstream_body_is_stable_bytes() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[]}"#);
        let request = UpstreamRequest {
            url: "https://upstream.invalid/v1/messages".to_owned(),
            headers: Headers::default(),
            body: body.clone(),
        };

        assert_eq!(request.body, body);
        assert_eq!(
            request.body.as_ptr(),
            body.as_ptr(),
            "clone は同じ領域を共有する"
        );
    }

    /// 分かれて届いた本文も 1 本に繋がる。
    #[tokio::test]
    async fn a_split_body_is_read_whole() {
        let body: BodyStream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"{\"error\":")),
            Ok(Bytes::from_static(b"\"invalid beta flag\"}")),
        ])
        .boxed();

        assert_eq!(
            String::from_utf8(collect_body(body).await.unwrap()).unwrap(),
            r#"{"error":"invalid beta flag"}"#
        );
    }

    /// 中身を見た後も、そのままクライアントへ返せる。
    #[tokio::test]
    async fn buffered_response_can_still_be_forwarded() {
        let resp = Response {
            status: 400,
            headers: headers(&[("content-type", "application/json")]),
            body: futures_util::stream::once(async { Ok(Bytes::from_static(b"{\"error\":1}")) })
                .boxed(),
        };

        let (resp, raw) = buffer(resp).await.unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), r#"{"error":1}"#);
        assert_eq!(resp.status, 400);
        assert_eq!(
            resp.headers.get("content-type"),
            Some("application/json"),
            "ヘッダは失わない"
        );
        assert_eq!(
            String::from_utf8(collect_body(resp.body).await.unwrap()).unwrap(),
            r#"{"error":1}"#,
            "読んだ後でも同じ本文を流せる"
        );
    }

    #[test]
    fn reads_the_model_name() {
        assert_eq!(model_of(&json!({"model": "m-1"})).unwrap(), "m-1");
        assert!(model_of(&json!({})).is_err());
        assert!(model_of(&json!({"model": ""})).is_err());
        assert!(model_of(&json!({"model": 42})).is_err());
    }

    #[test]
    fn the_model_is_rewritten_in_place() {
        let mut body = json!({"model": "m-1", "max_tokens": 8});
        rewrite_model(&mut body, "vendor.m-1");

        assert_eq!(body["model"], "vendor.m-1");
        assert_eq!(body["max_tokens"], 8, "他の項目は触らない");
    }

    async fn adapted(mode: ResponseMode, chunks: Vec<&'static [u8]>) -> Response {
        let response = Response {
            status: 200,
            headers: headers(&[("content-type", "text/event-stream; charset=utf-8")]),
            body: futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok(Bytes::from_static(chunk))),
            )
            .boxed(),
        };
        adapt_response(response, mode).await.unwrap()
    }

    /// 非ストリーム要求では、正規形 SSE の text・tool・usage・終了理由を
    /// 1 個の正規形 message JSON に組み立てる。chunk 境界は SSE 行と無関係でよい。
    #[tokio::test]
    async fn collects_messages_sse_into_one_json_message() {
        let response = adapted(
            ResponseMode::CollectMessagesSse,
            vec![
                b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"he",
                b"llo\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ],
        )
        .await;

        assert_eq!(
            response.headers.get("content-type"),
            Some("application/json")
        );
        let body: Value =
            serde_json::from_slice(&collect_body(response.body).await.unwrap()).unwrap();
        assert_eq!(body["type"], "message");
        assert_eq!(body["content"][0], json!({"type":"text","text":"hello"}));
        assert_eq!(
            body["content"][1],
            json!({"type":"tool_use","id":"tool_1","name":"read","input":{"path":"a"}})
        );
        assert_eq!(body["stop_reason"], "tool_use");
        assert_eq!(body["usage"], json!({"input_tokens":10,"output_tokens":7}));
    }

    /// streaming 中の正規形 error は、成功 message と混ぜず同じ error JSON 形で返す。
    #[tokio::test]
    async fn collects_an_sse_error_as_json_error() {
        let response = adapted(
            ResponseMode::CollectMessagesSse,
            vec![b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"failed\"}}\n\n"],
        )
        .await;

        let body: Value =
            serde_json::from_slice(&collect_body(response.body).await.unwrap()).unwrap();
        assert_eq!(
            response.headers.get("content-type"),
            Some("application/json")
        );
        assert_eq!(
            body,
            json!({"type":"error","error":{"type":"api_error","message":"failed"}})
        );
    }

    /// `message_stop` の無い切断は、不完全な message を成功扱いせず error JSON にする。
    #[tokio::test]
    async fn an_incomplete_sse_becomes_a_json_error() {
        let response = adapted(
            ResponseMode::CollectMessagesSse,
            vec![b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"content\":[]}}\n\n"],
        )
        .await;

        let body: Value =
            serde_json::from_slice(&collect_body(response.body).await.unwrap()).unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");
    }

    /// stream 要求では正規形 SSE をバッファせず、content-type と本文をそのまま流す。
    #[tokio::test]
    async fn passthrough_keeps_messages_sse() {
        let response = adapted(
            ResponseMode::Passthrough,
            vec![b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"],
        )
        .await;

        assert_eq!(
            response.headers.get("content-type"),
            Some("text/event-stream; charset=utf-8")
        );
        assert_eq!(
            collect_body(response.body).await.unwrap(),
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }
}
