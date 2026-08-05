//! upstream へ出るときの provider-neutral な HTTP 契約。
//!
//! ingress が作った Anthropic Messages 形式の正規形を [`EgressRequest`] で受け、
//! provider の [`crate::provider::Wire`] が送信可能な [`UpstreamRequest`] へ変換する。
//! 署名と実送信は同じ [`bytes::Bytes`] を参照し、JSON の再直列化で内容が変わらない。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

use crate::Result;

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
/// `body` は Anthropic Messages 形式の `serde_json::Value`。中立 IR は挟まない。
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

#[cfg(test)]
mod tests {
    use super::*;

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
            ("anthropic-version", "2023-06-01"),
            ("user-agent", "client/1"),
        ]);
        headers.strip_for_upstream();

        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![
                ("anthropic-version", "2023-06-01"),
                ("user-agent", "client/1")
            ]
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
}
