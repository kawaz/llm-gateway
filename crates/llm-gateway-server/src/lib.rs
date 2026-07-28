//! HTTP 層。Anthropic Messages API を話す口を生やす。
//!
//! 実運用のログから、クライアントが叩くのは 3 つと分かっている:
//! `POST /v1/messages` / `POST /v1/messages/count_tokens` / `GET /v1/models`。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::error;

use llm_gateway::credential::Persistence;
use llm_gateway::{Error, Gateway};

/// 転送するリクエストの上限。
///
/// 実測でクライアントは 250 KB 程度送ってくる。長い会話や画像が乗ると
/// 増えるので、上限は大きめに取る。無制限にしないのは、壊れた相手に
/// メモリを食い潰されないため。
const MAX_BODY: usize = 64 * 1024 * 1024;

pub fn router<P: Persistence + 'static>(gateway: Arc<Gateway<P>>) -> Router {
    Router::new()
        // namespace 付き。`/ns-personal/v1/messages` のように使う。
        .route("/{ns}/v1/messages", post(messages))
        .route("/{ns}/v1/messages/count_tokens", post(messages))
        .route("/{ns}/v1/models", get(models))
        // 付けなければ既定の namespace。単一の用途で使う分には意識しなくてよい。
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(messages))
        .route("/v1/models", get(models))
        .with_state(gateway)
}

/// パスの先頭から namespace 名を取り出す。
///
/// `/ns-personal/v1/messages` → `personal`。接頭辞を付けるのは、
/// namespace 名と API のパスを見分けるため (`/v1/...` と衝突しない)。
fn namespace_of(path: &str) -> &str {
    path.strip_prefix('/')
        .and_then(|rest| rest.split('/').next())
        .and_then(|first| first.strip_prefix("ns-"))
        .filter(|name| !name.is_empty())
        .unwrap_or(llm_gateway::config::DEFAULT_NAMESPACE)
}

/// upstream へ送るパス。namespace の部分を落とす。
///
/// `/ns-personal/v1/messages` → `/v1/messages`。upstream は namespace を
/// 知らないので、こちらの都合で付けた分は取り除いて渡す。
fn upstream_path(path: &str) -> &str {
    let Some(rest) = path.strip_prefix('/') else {
        return path;
    };
    match rest.split_once('/') {
        // `/ns-xxx` の分 (先頭の `/` と名前) を飛ばす。
        Some((first, _)) if first.starts_with("ns-") => &path[1 + first.len()..],
        _ => path,
    }
}

/// upstream へ渡して、返ってきたものをそのまま返す。
async fn messages<P: Persistence + 'static>(
    State(gateway): State<Arc<Gateway<P>>>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let uri = parts.uri.clone();

    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return client_error(StatusCode::BAD_REQUEST, &format!("本文を読めません: {e}")),
    };
    let json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return client_error(
                StatusCode::BAD_REQUEST,
                &format!("JSON として読めません: {e}"),
            );
        }
    };

    let headers = collect_headers(&parts.headers);
    let ns_name = namespace_of(uri.path()).to_owned();
    let Some(ns) = gateway.namespace(&ns_name) else {
        return unknown_namespace(&ns_name, &gateway.namespace_names());
    };
    if !ns.accepts(
        parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    ) {
        return unauthorized(&ns_name);
    }

    let path = upstream_path(uri.path()).to_owned();
    let query = uri.query().map(str::to_owned);

    match gateway
        .forward(ns, &path, query.as_deref(), json, headers)
        .await
    {
        Ok(upstream) => {
            let mut resp = Response::builder().status(upstream.status);
            for (name, value) in &upstream.headers {
                resp = resp.header(name, value);
            }
            // 本文は読まずに流す。SSE はここを通り抜けるだけ。
            resp.body(Body::from_stream(upstream.body))
                .unwrap_or_else(|e| {
                    error!(%e, "応答を組み立てられません");
                    client_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "応答を組み立てられません",
                    )
                })
        }
        Err(e) => error_response(&e),
    }
}

/// 使えるモデルの一覧。クライアントのモデル選択に出る。
///
/// 何が見えるかは namespace ごとに違う。
async fn models<P: Persistence + 'static>(
    State(gateway): State<Arc<Gateway<P>>>,
    request: Request,
) -> Response {
    let ns_name = namespace_of(request.uri().path()).to_owned();
    let Some(ns) = gateway.namespace(&ns_name) else {
        return unknown_namespace(&ns_name, &gateway.namespace_names());
    };
    if !ns.accepts(
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    ) {
        return unauthorized(&ns_name);
    }

    let data: Vec<Value> = gateway
        .models(ns)
        .await
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "type": "model"}))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

fn unknown_namespace(name: &str, known: &[&str]) -> Response {
    client_error(
        StatusCode::NOT_FOUND,
        &format!(
            "namespace `{name}` は設定されていません。使えるのは: {}",
            known.join(", ")
        ),
    )
}

fn unauthorized(ns: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": format!("namespace `{ns}` のトークンが違います"),
            },
        })),
    )
        .into_response()
}

fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_owned(), v.to_owned()))
        })
        .collect()
}

/// gateway の失敗をクライアントへ返す形にする。
///
/// Anthropic のエラー形式に合わせる。クライアントはこの形を読める。
fn error_response(e: &Error) -> Response {
    let (status, kind) = match e {
        Error::UnknownModel(_) => (StatusCode::NOT_FOUND, "not_found_error"),
        Error::AllUpstreamsFailed { .. } | Error::UpstreamUnreachable { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, "api_error")
        }
        Error::Credential { .. } | Error::Refresh { .. } => {
            (StatusCode::UNAUTHORIZED, "authentication_error")
        }
        Error::Config(_) | Error::Json(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
    };

    // 何が起きたかは全部書く。個人利用の proxy なので、隠すより
    // 原因が分かるほうが役に立つ。token は元から文言に含めていない。
    let message = match e {
        Error::AllUpstreamsFailed { model, attempts } => {
            let detail: Vec<String> = attempts.iter().map(ToString::to_string).collect();
            format!(
                "model `{model}` の経路が全て失敗しました: {}",
                detail.join(" / ")
            )
        }
        other => other.to_string(),
    };

    if status.is_server_error() {
        error!(%message, "リクエストを処理できません");
    }

    (
        status,
        Json(json!({"type": "error", "error": {"type": kind, "message": message}})),
    )
        .into_response()
}

fn client_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message},
        })),
    )
        .into_response()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use llm_gateway::Config;
    use llm_gateway::credential::{CredentialId, OauthTokens, Payload, StoredCredential};
    use tokio::net::TcpListener;

    struct StaticStore;

    impl Persistence for StaticStore {
        fn load(&self, _id: &CredentialId) -> llm_gateway::Result<StoredCredential> {
            Ok(StoredCredential::new(Payload::ClaudeOauth(OauthTokens {
                access_token: "tok".into(),
                refresh_token: "rt".into(),
                // 十分先。更新に入らせない。
                expired: "2099-01-01T00:00:00Z".into(),
                email: "a@b.c".into(),
                extra: Default::default(),
            })))
        }
        fn store(&self, _id: &CredentialId, _v: &StoredCredential) -> llm_gateway::Result<()> {
            Ok(())
        }
        fn list(&self) -> llm_gateway::Result<Vec<CredentialId>> {
            Ok(vec![])
        }
    }

    /// 受け取ったリクエストを覚えておく試験用 upstream。
    async fn fake_upstream(
        respond: impl Fn() -> (u16, String, Vec<(String, String)>) + Send + Sync + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let respond = Arc::new(respond);

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut buf = vec![0u8; 65536];
                    let _ = sock.read(&mut buf).await;
                    let (status, body, extra) = respond();
                    let mut head =
                        format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n", body.len());
                    for (k, v) in extra {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("connection: close\r\n\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        format!("http://{addr}")
    }

    /// gateway を立てて、その待ち受け先を返す。
    pub(crate) async fn serve(config_toml: &str) -> String {
        let config: Config = toml::from_str(config_toml).unwrap();
        config.validate().unwrap();
        let gateway = Arc::new(Gateway::new(&config, StaticStore).unwrap());
        gateway.refresh_models().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(gateway)).await;
        });
        format!("http://{addr}")
    }

    fn request_body() -> Value {
        json!({
            "model": "m",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    #[tokio::test]
    async fn forwards_and_returns_upstream_response() {
        let upstream = fake_upstream(|| {
            (
                200,
                r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#.to_owned(),
                vec![("content-type".into(), "application/json".into())],
            )
        })
        .await;

        let base = serve(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json",
            "upstream のヘッダを引き継ぐ"
        );
        assert!(resp.text().await.unwrap().contains("ok"));
    }

    /// SSE がそのまま流れる。イベントの形を崩さない。
    #[tokio::test]
    async fn streams_sse_through() {
        let sse = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

        let upstream = fake_upstream(move || {
            (
                200,
                sse.to_owned(),
                vec![("content-type".into(), "text/event-stream".into())],
            )
        })
        .await;

        let base = serve(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&json!({"model": "m", "stream": true, "messages": []}))
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(resp.text().await.unwrap(), sse, "1 バイトも変えない");
    }

    /// 設定に無いモデルは 404。どのモデルか分かる文言にする。
    #[tokio::test]
    async fn unknown_model_yields_404() {
        let base = serve(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m", "claude-opus-5", "claude-fable-5"]

[[routing]]
models = ["known"]
credentials = ["a"]
"#,
        )
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&json!({"model": "no-such-model", "messages": []}))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 404);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "not_found_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no-such-model"),
            "{body}"
        );
    }

    /// 経路が全滅したら 503。どこで何が起きたかを返す。
    #[tokio::test]
    async fn all_routes_down_yields_503_with_detail() {
        let down = fake_upstream(|| (503, "{}".to_owned(), vec![])).await;
        let base = serve(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{down}"
models = ["m"]

[[routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 503);
        let body: Value = resp.json().await.unwrap();
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains('a'), "どの経路が駄目だったか: {msg}");
        assert!(msg.contains("503"), "何が起きたか: {msg}");
    }

    /// upstream の 4xx はそのまま返す (gateway が握り潰さない)。
    #[tokio::test]
    async fn upstream_client_error_is_passed_through() {
        let upstream = fake_upstream(|| {
            (
                400,
                r#"{"type":"error","error":{"message":"max_tokens is required"}}"#.to_owned(),
                vec![("content-type".into(), "application/json".into())],
            )
        })
        .await;

        let base = serve(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400);
        assert!(
            resp.text()
                .await
                .unwrap()
                .contains("max_tokens is required"),
            "upstream の説明をそのまま渡す"
        );
    }

    #[tokio::test]
    async fn malformed_json_yields_400() {
        let base = serve(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m", "claude-opus-5", "claude-fable-5"]

[[routing]]
models = ["m"]
credentials = ["a"]
"#,
        )
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("content-type", "application/json")
            .body("{ not json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn lists_models() {
        let base = serve(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5", "claude-fable-5"]

[aliases]
fable = "claude-fable-*"
opus = "claude-opus-*"
"#,
        )
        .await;

        let body: Value = reqwest::get(format!("{base}/v1/models"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(body["object"], "list");
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["claude-fable-5", "claude-opus-5", "fable", "opus"],
            "実際のモデル名と、短い名前の両方を出す"
        );
    }

    /// count_tokens も同じ経路で捌く。パスは upstream へそのまま渡る。
    #[tokio::test]
    async fn count_tokens_is_routed_too() {
        let upstream = fake_upstream(|| (200, r#"{"input_tokens":42}"#.to_owned(), vec![])).await;
        let base = serve(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages/count_tokens"))
            .json(&request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        assert!(resp.text().await.unwrap().contains("42"));
    }
}

#[cfg(test)]
mod e2e_namespace_tests {
    use super::tests::serve;
    use serde_json::Value;

    /// 2 つの namespace を持つ設定。見えるモデルが違う。
    const TWO_NS: &str = r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5", "claude-fable-5", "gpt-5.6-sol"]

[ns.personal]
[ns.personal.filter]
exclude = ["gpt-*"]
[ns.personal.aliases]
opus = "claude-opus-*"

[ns.work]
[ns.work.filter]
exclude = ["claude-fable-*"]

[ns.locked]
auth_token = "secret-token"
"#;

    async fn ids(base: &str, path: &str) -> Vec<String> {
        let body: Value = reqwest::get(format!("{base}{path}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// 同じ upstream を見ていても、namespace ごとに見えるモデルが違う。
    #[tokio::test]
    async fn each_namespace_sees_its_own_models() {
        let base = serve(TWO_NS).await;

        let personal = ids(&base, "/ns-personal/v1/models").await;
        assert!(personal.contains(&"claude-fable-5".to_owned()));
        assert!(
            !personal.iter().any(|m| m.starts_with("gpt-")),
            "personal は gpt を隠している: {personal:?}"
        );
        assert!(
            personal.contains(&"opus".to_owned()),
            "エイリアスも namespace ごと"
        );

        let work = ids(&base, "/ns-work/v1/models").await;
        assert!(work.contains(&"gpt-5.6-sol".to_owned()));
        assert!(
            !work.contains(&"claude-fable-5".to_owned()),
            "work は fable を隠している: {work:?}"
        );
        assert!(!work.contains(&"opus".to_owned()), "エイリアスは共有しない");
    }

    /// 設定していない namespace は 404。使えるものを教える。
    #[tokio::test]
    async fn unknown_namespace_is_rejected() {
        let base = serve(TWO_NS).await;
        let resp = reqwest::get(format!("{base}/ns-nope/v1/models"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let body: Value = resp.json().await.unwrap();
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains("nope"), "{msg}");
        assert!(msg.contains("personal"), "使えるものを挙げる: {msg}");
    }

    /// トークンを設定した namespace は、合っていないと通さない。
    #[tokio::test]
    async fn namespace_token_is_checked() {
        let base = serve(TWO_NS).await;
        let client = reqwest::Client::new();

        let without = client
            .get(format!("{base}/ns-locked/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(without.status(), 401, "トークン無しは通さない");

        let wrong = client
            .get(format!("{base}/ns-locked/v1/models"))
            .header("authorization", "Bearer nope")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401);

        for value in ["Bearer secret-token", "secret-token"] {
            let ok = client
                .get(format!("{base}/ns-locked/v1/models"))
                .header("authorization", value)
                .send()
                .await
                .unwrap();
            assert_eq!(ok.status(), 200, "{value} は通す");
        }
    }

    /// トークンを設定していない namespace は誰でも使える。
    #[tokio::test]
    async fn namespace_without_token_is_open() {
        let base = serve(TWO_NS).await;
        let resp = reqwest::get(format!("{base}/ns-personal/v1/models"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::*;

    #[test]
    fn extracts_namespace_from_path() {
        assert_eq!(namespace_of("/ns-personal/v1/messages"), "personal");
        assert_eq!(namespace_of("/ns-work/v1/models"), "work");
    }

    /// 接頭辞が無ければ既定。単一の用途では namespace を意識せずに済む。
    #[test]
    fn plain_path_uses_the_default() {
        assert_eq!(namespace_of("/v1/messages"), "default");
        assert_eq!(namespace_of("/v1/models"), "default");
        assert_eq!(namespace_of("/"), "default");
        assert_eq!(namespace_of(""), "default");
    }

    /// `ns-` だけでは名前にならない。
    #[test]
    fn empty_namespace_name_falls_back() {
        assert_eq!(namespace_of("/ns-/v1/messages"), "default");
    }

    /// 接頭辞を付けるのは API のパスと見分けるため。
    /// `v1` を namespace 名と誤認しない。
    #[test]
    fn api_path_is_not_mistaken_for_a_namespace() {
        assert_eq!(namespace_of("/v1/messages/count_tokens"), "default");
    }

    #[test]
    fn strips_namespace_before_forwarding() {
        assert_eq!(upstream_path("/ns-personal/v1/messages"), "/v1/messages");
        assert_eq!(
            upstream_path("/ns-work/v1/messages/count_tokens"),
            "/v1/messages/count_tokens"
        );
    }

    /// 接頭辞が無ければそのまま。
    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(upstream_path("/v1/messages"), "/v1/messages");
        assert_eq!(upstream_path("/v1/models"), "/v1/models");
    }

    /// upstream は namespace を知らない。渡すパスに残してはいけない。
    #[test]
    fn upstream_never_sees_the_namespace() {
        for path in [
            "/ns-personal/v1/messages",
            "/ns-a/v1/models",
            "/ns-x/v1/messages/count_tokens",
        ] {
            assert!(
                !upstream_path(path).contains("ns-"),
                "{path} → {}",
                upstream_path(path)
            );
        }
    }
}
