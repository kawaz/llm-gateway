//! OpenAI 応答をクライアントへ流す前の採用判定。
//!
//! 最初の event を待つ間 upstream が沈黙しても、ここでは時間の上限を設けない。
//! 待ちすぎの検知は HTTP クライアントの接続/読み取り timeout に任せる。
//! [`MAX_EVENT`] は「異常に大きい先頭 event」への保護であって、遅さの検知
//! ではないので、両者を混同しない (DR-0014 §9)。

use bytes::Bytes;
use futures_util::StreamExt as _;
use serde_json::Value;

use crate::Result;
use crate::denial::{DEFAULT_BACKOFF, Denial, Reason, Scope};
use crate::egress::{BodyStream, Response};
use crate::provider::{Admission, ClientError, ResponseAdmission};

use super::response::MAX_EVENT;

pub struct OpenAiResponseAdmission;

impl ResponseAdmission for OpenAiResponseAdmission {
    fn admit<'a>(
        &'a self,
        response: Response,
        model: &'a str,
        observed_at: i64,
    ) -> crate::egress::BoxFuture<'a, Result<Admission>> {
        Box::pin(async move { admit(response, model, observed_at).await })
    }
}

async fn admit(response: Response, model: &str, observed_at: i64) -> Result<Admission> {
    let Response {
        status,
        headers,
        mut body,
    } = response;
    let mut prefix = Vec::new();

    loop {
        if let Some(end) = event_end(&prefix) {
            if end > MAX_EVENT {
                return Err(crate::Error::Config(
                    "OpenAI 応答の先頭 event が大きすぎます".to_owned(),
                ));
            }
            let remainder = prefix.split_off(end);
            body = replay(remainder, body);
            break;
        }
        if prefix.len() > MAX_EVENT {
            return Err(crate::Error::Config(
                "OpenAI 応答の先頭 event が大きすぎます".to_owned(),
            ));
        }
        let Some(chunk) = body.next().await else {
            break;
        };
        prefix.extend_from_slice(&chunk?);
    }

    let replayed = replay(prefix.clone(), body);
    let response = Response {
        status,
        headers,
        body: replayed,
    };
    let Some(error) = first_error(&prefix)? else {
        return Ok(Admission::Admitted(response));
    };
    let client_error = (status / 100 == 2).then(|| classify_client_error(&error));

    let denial = (error.kind == "overloaded_error").then(|| Denial {
        until: observed_at + DEFAULT_BACKOFF,
        reason: Reason::Busy,
        scope: Scope::Model(model.to_owned()),
    });
    Ok(Admission::Rejected {
        response,
        reason: error.message,
        denial,
        client_error,
    })
}

fn classify_client_error(error: &ErrorEvent) -> ClientError {
    let kind_lower = error.kind.to_ascii_lowercase();
    let message_lower = error.message.to_ascii_lowercase();
    let (status, kind) = if kind_lower.contains("overload") || message_lower.contains("overload") {
        (529, "overloaded_error")
    } else if kind_lower.contains("rate_limit") || message_lower.contains("rate limit") {
        (429, "rate_limit_error")
    } else if kind_lower.contains("invalid_request")
        || message_lower.contains("context window")
        || message_lower.contains("context length")
    {
        (400, "invalid_request_error")
    } else {
        (502, "api_error")
    };
    ClientError {
        status,
        kind: kind.to_owned(),
        message: error.message.clone(),
    }
}

fn replay(prefix: Vec<u8>, rest: BodyStream) -> BodyStream {
    if prefix.is_empty() {
        return rest;
    }
    futures_util::stream::once(async move { Ok(Bytes::from(prefix)) })
        .chain(rest)
        .boxed()
}

fn event_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
        .or_else(|| {
            bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
        })
}

struct ErrorEvent {
    kind: String,
    message: String,
}

fn first_error(bytes: &[u8]) -> Result<Option<ErrorEvent>> {
    let Some(end) = event_end(bytes) else {
        return Ok(None);
    };
    let mut data = Vec::new();
    for line in bytes[..end].split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(value) = line.strip_prefix(b"data:") else {
            continue;
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(value);
    }
    if data.is_empty() {
        return Ok(None);
    }
    let event: Value = serde_json::from_slice(&data)?;
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return Ok(None);
    }
    let error = &event["error"];
    Ok(Some(ErrorEvent {
        kind: error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("api_error")
            .to_owned(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("OpenAI response failed")
            .to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::Headers;
    use futures_util::stream;

    fn response(chunks: Vec<&'static [u8]>) -> Response {
        Response {
            status: 200,
            headers: Headers::new(vec![(
                "content-type".to_owned(),
                "text/event-stream".to_owned(),
            )]),
            body: stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok(Bytes::from_static(chunk))),
            )
            .boxed(),
        }
    }

    async fn body_of(response: Response) -> Vec<u8> {
        crate::egress::collect_body(response.body).await.unwrap()
    }

    /// 正常な最初の event を確認したら、後続を待たず、読んだ bytes も変えずに採用する。
    #[tokio::test]
    async fn admits_after_the_first_success_event_without_buffering_the_rest() {
        let first = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        let outcome = admit(response(vec![first, b"later"]), "m", 100)
            .await
            .unwrap();
        let Admission::Admitted(response) = outcome else {
            panic!("正常 event は採用する");
        };
        assert_eq!(
            body_of(response).await,
            [first.as_slice(), b"later"].concat()
        );
    }

    /// 採用後に届く error は fallback できないため、本文を変えずクライアントへ流す。
    #[tokio::test]
    async fn leaves_an_error_after_the_first_event_in_the_stream() {
        let first = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        let later = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"late\"}}\n\n";
        let outcome = admit(response(vec![first, later]), "m", 100).await.unwrap();
        let Admission::Admitted(response) = outcome else {
            panic!("送出開始後の error では切り替えない");
        };
        assert_eq!(body_of(response).await, [first.as_slice(), later].concat());
    }

    /// event が chunk 境界を跨いでも、空行まで揃えてから overloaded を判定する。
    #[tokio::test]
    async fn rejects_an_overloaded_error_split_across_chunks() {
        let outcome = admit(
            response(vec![
                b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_",
                b"error\",\"message\":\"busy\"}}\n\n",
            ]),
            "gpt",
            100,
        )
        .await
        .unwrap();
        let Admission::Rejected { denial, reason, .. } = outcome else {
            panic!("混雑は別経路へ回す");
        };
        assert_eq!(reason, "busy");
        assert_eq!(
            denial,
            Some(Denial {
                until: 100 + DEFAULT_BACKOFF,
                reason: Reason::Busy,
                scope: Scope::Model("gpt".to_owned()),
            })
        );
    }

    /// リクエスト自体の誤りは別経路でも直らないため、fallback せずクライアントへ返す。
    #[tokio::test]
    async fn rejects_a_client_request_error() {
        let raw = b"event: error\r\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad request\"}}\r\n\r\n";
        let outcome = admit(response(vec![raw]), "m", 100).await.unwrap();
        let Admission::Rejected { client_error, .. } = outcome else {
            panic!("依頼の誤りは status を補って返す");
        };
        assert_eq!(client_error.unwrap().status, 400);
    }

    /// 本文内エラーの意味を Anthropic error type と HTTP status の組へ正規化する。
    #[test]
    fn maps_in_body_error_categories() {
        let cases = [
            ("overloaded_error", "busy", 529, "overloaded_error"),
            (
                "server_error",
                "rate limit exceeded",
                429,
                "rate_limit_error",
            ),
            (
                "invalid_request_error",
                "context window exceeded",
                400,
                "invalid_request_error",
            ),
            ("mystery", "upstream detail", 502, "api_error"),
        ];
        for (source_kind, message, status, kind) in cases {
            assert_eq!(
                classify_client_error(&ErrorEvent {
                    kind: source_kind.to_owned(),
                    message: message.to_owned(),
                }),
                ClientError {
                    status,
                    kind: kind.to_owned(),
                    message: message.to_owned(),
                }
            );
        }
    }

    /// 元から HTTP エラーなら status・headers・本文を生透過するため変換情報を付けない。
    #[tokio::test]
    async fn leaves_an_http_error_denial_unchanged() {
        let raw = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n";
        let mut source = response(vec![raw]);
        source.status = 429;
        source.headers.set("retry-after", "30");
        let Admission::Rejected {
            response,
            client_error,
            ..
        } = admit(source, "m", 100).await.unwrap()
        else {
            panic!("HTTP denial は別経路へ回す");
        };
        assert_eq!(client_error, None);
        assert_eq!(response.status, 429);
        assert_eq!(response.headers.get("retry-after"), Some("30"));
        assert_eq!(body_of(response).await, raw);
    }

    /// 明示的な failure が無い空本文を、推測で route failure にしない。
    #[tokio::test]
    async fn admits_an_empty_body() {
        assert!(matches!(
            admit(response(vec![]), "m", 100).await.unwrap(),
            Admission::Admitted(_)
        ));
    }

    /// 同時に複数応答を判定しても、先頭 bytes やモデル別 denial が混ざらない。
    #[tokio::test]
    async fn concurrent_admissions_keep_each_response_separate() {
        let runs = (0..16).map(|index| async move {
            let model = format!("m-{index}");
            let raw = format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"busy-{index}\"}}}}\n\n"
            );
            let response = Response {
                status: 200,
                headers: Headers::default(),
                body: stream::once(async move { Ok(Bytes::from(raw)) }).boxed(),
            };
            let Admission::Rejected { denial, reason, .. } =
                admit(response, &model, 100).await.unwrap()
            else {
                panic!("混雑は拒否する");
            };
            assert_eq!(reason, format!("busy-{index}"));
            assert_eq!(denial.unwrap().scope, Scope::Model(model));
        });
        futures_util::future::join_all(runs).await;
    }

    /// 先頭 event の上限を超えた応答は、クライアントへ流す前の protocol failure にする。
    #[tokio::test]
    async fn rejects_an_event_larger_than_the_limit() {
        let large = vec![b'x'; MAX_EVENT + 1];
        let response = Response {
            status: 200,
            headers: Headers::default(),
            body: stream::once(async move { Ok(Bytes::from(large)) }).boxed(),
        };
        assert!(admit(response, "m", 100).await.is_err());
    }
}
