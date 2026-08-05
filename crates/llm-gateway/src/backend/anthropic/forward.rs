//! upstream への転送の入口 (facade)。
//!
//! 組み立てと送信は preset の [`crate::provider::Wire`] が、認証は
//! [`crate::provider::Auth`] が持つ。ここは呼び出し側が持っている形
//! (`Value` のボディ + ヘッダの組) を [`EgressRequest`] に載せ替え、返って
//! きた応答を旧来の形に戻すだけ。
//!
//! 要求の側が `Value` を受け取って組み立て直しているのは、ここが経路ごとに
//! 何度も呼ばれるため。理由は受け取り口 (`llm-gateway-server` の `messages`)
//! に書いてある。

use futures_util::StreamExt as _;
use serde_json::Value;

use crate::Result;
use crate::credential::Credential;
use crate::egress::EgressRequest;
use crate::provider::Preset;

use super::Headers;

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

/// core の形から、受け取り口が読む形へ。
///
/// 違うのはヘッダの入れ物だけ。ヘッダの組をそのまま総なめする読み手
/// (`llm-gateway-server`) が居るので、その形で渡す。
impl From<crate::egress::Response> for Response {
    fn from(resp: crate::egress::Response) -> Self {
        Self {
            status: resp.status,
            headers: resp.headers.as_slice().to_vec(),
            body: resp.body,
        }
    }
}

/// 応答の本文。
pub type BodyStream = crate::egress::BodyStream;

/// 転送する。
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
    path: &str,
    query: Option<&str>,
    body: Value,
    headers: Headers,
) -> Result<Response> {
    let mut request = preset.wire().encode(EgressRequest {
        path: path.to_owned(),
        query: query.map(str::to_owned),
        body,
        headers,
    })?;
    preset.auth().authorize(credential, &mut request)?;

    Ok(preset.wire().send(http, request).await?.into())
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

    /// core の応答を受け取り口の形へ移しても、中身は失われない。
    #[tokio::test]
    async fn converting_from_the_core_response_keeps_everything() {
        let core = crate::egress::Response {
            status: 429,
            headers: Headers::new(vec![("retry-after".to_owned(), "30".to_owned())]),
            body: futures_util::stream::once(async { Ok(bytes::Bytes::from_static(b"body")) })
                .boxed(),
        };

        let resp: Response = core.into();
        assert_eq!(resp.status, 429);
        assert_eq!(
            resp.headers,
            vec![("retry-after".to_owned(), "30".to_owned())]
        );
        assert_eq!(collect_body(resp.body).await.unwrap(), b"body");
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
}
