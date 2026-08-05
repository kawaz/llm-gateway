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
) -> Result<Response> {
    let mut request = preset.wire().encode(request)?;
    preset.auth().authorize(credential, &mut request)?;
    preset.wire().send(http, request).await
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
        .ok_or_else(|| Error::Config("リクエストに model がありません".to_owned()))
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
}
