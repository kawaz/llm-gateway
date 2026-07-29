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
    headers.strip_for_upstream();
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
        async move { Ok(bytes::Bytes::from(raw)) }
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

/// この状態なら別の upstream を試す価値があるか。
///
/// 経路が断たれている場合だけ切り替える。応答を持ち回る値打ちのない失敗で、
/// 中身は捨てる。断られた応答を残したまま切り替えるものは
/// [`is_route_denial`] が見る。
pub fn should_try_next(status: u16) -> bool {
    // 501 (未実装) は除く。別の経路に替えても実装されていないものは動かない。
    matches!(status, 500 | 502..=504)
}

/// この経路には断られたが、別の経路なら通りうるか。
///
/// 上限もトークンの有効性も混み具合も、この経路の向こう側 (アカウントと
/// 宛先) に付く。並んでいる認証情報は別のアカウントで、宛先も
/// Bedrock / Anthropic / 中継と分かれているので、ここが断ったことは
/// 次が断ることを意味しない。
///
/// - 401 / 403: upstream との認証の話。クライアント側の認証は gateway が
///   namespace のトークンで別に確かめている
/// - 429: 上限はアカウント単位
/// - 529: 宛先の混み具合。宛先が分かれている構成では、片方が詰まっていても
///   もう片方は空いている (実測 2026-07-29)
///
/// 応答は捨てずに持ち回る。全部断られたときは、これをそのまま返す。
pub fn is_route_denial(status: u16) -> bool {
    matches!(status, 401 | 403 | 429 | 529)
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

    /// この経路に断られた分は、経路断とは別に扱う (応答を持ち回る側)。
    #[test]
    fn route_denials_are_told_apart_from_outages() {
        for status in [401, 403, 429, 529] {
            assert!(is_route_denial(status), "{status} はこの経路が断った");
            assert!(!should_try_next(status), "{status} は経路断ではない");
        }
    }

    /// リクエスト側の誤りは切り替えても直らない。
    #[test]
    fn does_not_switch_on_client_error() {
        for status in [400, 404, 422] {
            assert!(!should_try_next(status), "{status} は次を試さない");
            assert!(!is_route_denial(status), "{status} は経路のせいではない");
        }
    }

    /// 経路断と成功は、この経路に断られたわけではない。
    #[test]
    fn outages_and_success_are_not_route_denials() {
        for status in [200, 500, 502, 503, 504] {
            assert!(!is_route_denial(status), "{status}");
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

    /// 中身を見た後も、そのままクライアントへ返せる。
    #[tokio::test]
    async fn buffered_response_can_still_be_forwarded() {
        let resp = Response {
            status: 400,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: futures_util::stream::iter(vec![
                Ok(bytes::Bytes::from_static(b"{\"error\":")),
                Ok(bytes::Bytes::from_static(b"\"invalid beta flag\"}")),
            ])
            .boxed(),
        };

        let (resp, raw) = buffer(resp).await.unwrap();
        assert_eq!(
            String::from_utf8(raw).unwrap(),
            r#"{"error":"invalid beta flag"}"#
        );
        assert_eq!(resp.status, 400);
        assert_eq!(resp.headers.len(), 1, "ヘッダは失わない");
        assert_eq!(
            String::from_utf8(collect_body(resp.body).await.unwrap()).unwrap(),
            r#"{"error":"invalid beta flag"}"#,
            "読んだ後でも同じ本文を流せる"
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
