//! upstream への転送。
//!
//! ボディは `model` だけ差し替え、あとはそのまま渡す。応答はヘッダを受け取った
//! 時点で返し、本文はストリームのまま流す — Claude 系は SSE の形式が
//! upstream 間で同じなので、解釈せずバイト列のまま中継できる。

use futures_util::StreamExt as _;
use serde_json::Value;

use crate::credential::Credential;
use crate::{Error, Result};

use super::{Headers, Provider};

/// upstream からの応答。本文はまだ読んでいない。
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
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

/// 応答の本文。
pub type BodyStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<bytes::Bytes>> + Send>>;

/// 転送する。
///
/// 返るのは upstream のヘッダを受け取った時点。本文はまだ流れていないので、
/// ここまでの失敗なら別の upstream に切り替えられる。
pub async fn send(
    http: &reqwest::Client,
    provider: &dyn Provider,
    credential: Option<&Credential>,
    path: &str,
    query: Option<&str>,
    mut body: Value,
    mut headers: Headers,
) -> Result<Response> {
    headers.strip_hop_by_hop();
    provider.authorize(&mut headers, credential)?;
    provider.adapt(&mut body, &mut headers);

    let mut url = format!("{}{}", provider.base_url().trim_end_matches('/'), path);
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }

    let mut req = http.post(&url).json(&body);
    for (k, v) in headers.iter() {
        req = req.header(k, v);
    }

    let resp = req.send().await.map_err(|e| Error::UpstreamUnreachable {
        provider: provider.name().to_owned(),
        source: e,
    })?;

    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter(|(k, _)| !is_hop_by_hop(k.as_str()))
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_owned(), v.to_owned()))
        })
        .collect();

    let body = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|e| Error::Config(format!("応答の読み取りが途切れました: {e}"))))
        .boxed();

    Ok(Response {
        status,
        headers,
        body,
    })
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

/// この状態なら別の upstream を試す価値があるか。
///
/// 経路が断たれている場合だけ切り替える。429 では切り替えない — レート制限は
/// 別の経路でも同じように当たる可能性が高く、切り替えると単に負荷が移るだけ。
pub fn should_try_next(status: u16) -> bool {
    // 501 (未実装) は除く。別の経路に替えても実装されていないものは動かない。
    matches!(status, 500 | 502..=504)
}

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

    /// 経路が断たれた状態だけ次へ回す。
    #[test]
    fn switches_only_on_upstream_outage() {
        for status in [500, 502, 503, 504] {
            assert!(should_try_next(status), "{status} は経路断とみなす");
        }
    }

    /// レート制限では切り替えない。別の経路でも同じように当たるので、
    /// 負荷を移すだけで解決しない。
    #[test]
    fn does_not_switch_on_rate_limit() {
        assert!(!should_try_next(429));
    }

    /// リクエスト側の誤りは切り替えても直らない。
    #[test]
    fn does_not_switch_on_client_error() {
        for status in [400, 401, 403, 404, 422] {
            assert!(!should_try_next(status), "{status} は次を試さない");
        }
    }

    /// 未実装は別の経路でも未実装。
    #[test]
    fn does_not_switch_on_not_implemented() {
        assert!(!should_try_next(501));
    }

    #[test]
    fn does_not_switch_on_success() {
        for status in [200, 201, 204] {
            assert!(!should_try_next(status));
        }
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
