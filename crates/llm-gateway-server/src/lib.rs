//! HTTP 層。Anthropic Messages API を話す口を生やす。
//!
//! 実運用のログから、クライアントが叩くのは 3 つと分かっている:
//! `POST /v1/messages` / `POST /v1/messages/count_tokens` / `GET /v1/models`。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::{Instrument as _, error};

use llm_gateway::config::{Authorization, Namespace};
use llm_gateway::credential::Persistence;
use llm_gateway::{Error, Gateway, relay};

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
        // gateway 自身の機能はここの下にまとめる (DR-0006)。
        .route("/llm-gateway/healthz", get(healthz))
        .route("/llm-gateway/usage", get(usage))
        .with_state(gateway)
}

/// 生きているかだけを返す。
///
/// 前段のロードバランサが数秒ごとに叩く前提なので、認証も namespace も
/// 持たず、credential にも upstream にも触らない。
///
/// 業務のエンドポイントを死活監視に使うと、そのエンドポイントの認証方針が
/// 監視側に縛られる。実際 `/v1/models` を監視に使っていたために、
/// 既定 namespace へ認証をかけると監視が 401 で落ちて全断する状態になっていた。
/// 責務が違うものを同じ口にしない。
///
/// `/llm-gateway/` の下に置くのは、upstream の API 名と衝突しないため
/// (DR-0006)。裸で `/healthz` に置くと、upstream がその名前を使い始めたときに
/// こちらが避難することになる。
async fn healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// credential ごとの利用状況を返す。
///
/// 認証を掛けないのは healthz と同じ扱い (DR-0007)。出すのは使用率・
/// リセット時刻・フラグだけで、token も organization id も出さない。
///
/// `?refresh=true` のときだけ能動プローブに入る。既定を便乗のみにするのは、
/// usage の確認が usage を勝手に消費する構図を避けるため。
async fn usage<P: Persistence + 'static>(
    State(gateway): State<Arc<Gateway<P>>>,
    request: Request,
) -> Response {
    let refresh = wants_refresh(request.uri().query());
    Json(gateway.usage_report(refresh).await).into_response()
}

/// `?refresh=true` が付いているか。
///
/// 値を見るのは、`?refresh=false` を「付いている」と読むと、消費しない側を
/// 選んだつもりの相手に実リクエストを投げることになるため。
fn wants_refresh(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .any(|(k, v)| k == "refresh" && matches!(v, "true" | "1"))
    })
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

    // このリクエストに番号を振る。以降のログは全部この下に入るので、
    // 節目 (本文を受け取った / ヘッダが返った / 流し始めた / 終端) を
    // 突き合わせられる。番号を振るのは本文を読む前 — 読むのにかかった
    // 時間も、この番号の下に残したい。
    let span = relay::request_span();

    let receiving = Instant::now();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return client_error(StatusCode::BAD_REQUEST, &format!("本文を読めません: {e}")),
    };
    // 大きさは受け取った実バイト数で数える。Content-Length は手前の
    // プロキシが chunked で渡してくると付かないが、こちらは必ず取れる。
    relay::record_request_body(&span, bytes.len(), receiving.elapsed());

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
    if let Some(denied) = rejection(ns, &ns_name, &parts.headers) {
        return denied;
    }

    let path = upstream_path(uri.path()).to_owned();
    let query = uri.query().map(str::to_owned);

    match gateway
        .forward(ns, &ns_name, &path, query.as_deref(), json, headers)
        .instrument(span.clone())
        .await
    {
        Ok(upstream) => {
            let mut resp = Response::builder().status(upstream.status);
            for (name, value) in &upstream.headers {
                resp = resp.header(name, value);
            }
            // 本文は読まずに流す。SSE はここを通り抜けるだけ。
            // 包むのは終端 (流し切った / 途切れた / 中断された) を残すため。
            resp.body(Body::from_stream(relay::observe(upstream.body, span)))
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
    if let Some(denied) = rejection(ns, &ns_name, request.headers()) {
        return denied;
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

/// トークンを検査して、通さないなら返す応答を作る。
///
/// 文言を分けるのは、送っているトークンが疑わしいのか、gateway 側の設定が
/// 足りないのかで打つ手が違うから。同じ文言だと、正しいトークンを送っている
/// 利用者が「合っているはずなのに」で止まる (DR-0006)。
fn rejection(ns: &Namespace, name: &str, headers: &HeaderMap) -> Option<Response> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let message = match ns.authorize(presented) {
        Authorization::Accepted => return None,
        Authorization::WrongToken => format!("namespace `{name}` のトークンが違います"),
        Authorization::NoTokenConfigured => format!(
            "namespace `{name}` に auth_token が設定されていないので、誰も通せません。\
             設定の `[ns.{name}]` に auth_token を書いてください"
        ),
    };

    Some(
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": message},
            })),
        )
            .into_response(),
    )
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
    use std::sync::{Mutex, OnceLock};
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
    pub(crate) async fn fake_upstream(
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

    /// 途中で切れる upstream。
    ///
    /// 宣言した長さより短い本文を書いて、そのまま閉じる。SSE の中継中に
    /// upstream が落ちたときと同じ形 (ヘッダは通り、本文が終わらない)。
    async fn truncating_upstream(head: &'static str, declared: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut buf = vec![0u8; 65536];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 X\r\ncontent-type: text/event-stream\r\n\
content-length: {declared}\r\n\r\n{head}"
                            )
                            .as_bytes(),
                        )
                        .await;
                    let _ = sock.flush().await;
                    // 残りを書かずに閉じる。
                });
            }
        });

        format!("http://{addr}")
    }

    /// 試験中のログを集める先。
    ///
    /// 記録するのはクライアントへ本文を流す側 (別のタスク) なので、
    /// スレッドローカルでは捕まえられない。プロセスに 1 つだけ差し込む。
    fn captured_logs() -> &'static Mutex<Vec<u8>> {
        static LOGS: OnceLock<&'static Mutex<Vec<u8>>> = OnceLock::new();
        LOGS.get_or_init(|| {
            let logs: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
            let subscriber = tracing_subscriber::fmt()
                .with_writer(Sink(logs))
                .with_ansi(false)
                .without_time()
                .finish();
            // 既に誰かが差し込んでいても構わない (その場合は集まらないだけ)。
            let _ = tracing::subscriber::set_global_default(subscriber);
            logs
        })
    }

    #[derive(Clone)]
    struct Sink(&'static Mutex<Vec<u8>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// 試験で名乗るトークン。
    pub(crate) const TOKEN: &str = "test-token";

    /// 認証を書いた `[ns.default]` を足して立てる。
    ///
    /// `auth_token` の無い namespace は誰も通さない (DR-0006) ので、認証以外を
    /// 見る試験でも既定 namespace には token が要る。名乗る側は [`authed`]。
    pub(crate) async fn serve_with_default_ns(config_toml: &str) -> String {
        serve(&format!(
            "{config_toml}\n[ns.default]\nauth_token = \"{TOKEN}\"\n"
        ))
        .await
    }

    /// トークンを名乗る。
    pub(crate) fn authed(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(TOKEN)
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

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&request_body()),
        )
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

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&json!({"model": "m", "stream": true, "messages": []})),
        )
        .send()
        .await
        .unwrap();

        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(resp.text().await.unwrap(), sse, "1 バイトも変えない");
    }

    /// 中継の途中で upstream が切れたら、ログに残る。
    ///
    /// ヘッダを受け取った時点のログだけでは「最後まで届いたか」が分からない。
    /// 同じ番号 (req) の終了ログまで見て、初めて切り分けられる。
    #[tokio::test]
    async fn broken_stream_is_recorded() {
        let logs = captured_logs();

        // 100 バイトあると言って 20 バイト程度で閉じる。
        let upstream = truncating_upstream(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n",
            100,
        )
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&json!({"model": "m", "stream": true, "messages": []})),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), 200, "ヘッダは通る");

        // 本文はここで途切れる。クライアントが受け取り損ねた時点では、
        // 中継側は既に記録を終えている (記録してから先へ渡すため)。
        assert!(resp.text().await.is_err(), "本文は最後まで来ない");

        let text = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
        let broken = text
            .lines()
            .find(|l| l.contains("転送が途切れました"))
            .unwrap_or_else(|| panic!("途切れた記録が無い:\n{text}"));
        assert!(broken.contains("bytes="), "どこまで流したか: {broken}");
        assert!(broken.contains("elapsed_ms="), "かかった時間: {broken}");
        assert!(
            broken.contains("body_bytes="),
            "受け取った本文の大きさ (手前のアクセスログと突き合わせる): {broken}"
        );

        // span には番号の後ろに他の値も並ぶので、番号はそこで切る。
        let req = broken
            .split_once("req=")
            .and_then(|(_, rest)| rest.split([',', '}']).next())
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("番号が振られていない: {broken}"));
        assert!(
            text.lines()
                .any(|l| l.contains("ヘッダを受け取りました") && l.contains(&format!("req={req}"))),
            "開始と終了が同じ番号で対になる (req={req}):\n{text}"
        );
    }

    /// 設定に無いモデルは 404。どのモデルか分かる文言にする。
    #[tokio::test]
    async fn unknown_model_yields_404() {
        let base = serve_with_default_ns(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m", "claude-opus-5", "claude-fable-5"]

[[ns.default.routing]]
models = ["known"]
credentials = ["a"]
"#,
        )
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&json!({"model": "no-such-model", "messages": []})),
        )
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
        let base = serve_with_default_ns(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{down}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&request_body()),
        )
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

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&request_body()),
        )
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
        let base = serve_with_default_ns(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m", "claude-opus-5", "claude-fable-5"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#,
        )
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .header("content-type", "application/json")
                .body("{ not json"),
        )
        .send()
        .await
        .unwrap();

        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn lists_models() {
        let base = serve_with_default_ns(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5", "claude-fable-5"]

[ns.default.aliases]
fable = "claude-fable-*"
opus = "claude-opus-*"
"#,
        )
        .await;

        let body: Value = authed(reqwest::Client::new().get(format!("{base}/v1/models")))
            .send()
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
        let base = serve_with_default_ns(&format!(
            r#"
[credentials.a]
type = "relay"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
credentials = ["a"]
"#
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages/count_tokens"))
                .json(&request_body()),
        )
        .send()
        .await
        .unwrap();

        assert_eq!(resp.status(), 200);
        assert!(resp.text().await.unwrap().contains("42"));
    }
}

#[cfg(test)]
mod usage_tests {
    use super::tests::{authed, fake_upstream, serve_with_default_ns};
    use super::*;
    use serde_json::json;

    /// 実測された unified ヘッダ (DR-0007 の表)。
    fn unified_headers() -> Vec<(String, String)> {
        vec![
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "0.71".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".into(),
                "1785344400".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization".into(),
                "0.3".into(),
            ),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason".into(),
                "out_of_credits".into(),
            ),
        ]
    }

    async fn get(base: &str, path: &str) -> Value {
        reqwest::get(format!("{base}{path}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    fn entry<'a>(report: &'a Value, name: &str) -> &'a Value {
        report["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("{name} が一覧にない: {report}"))
    }

    /// 使ったことのない credential も、名前と理由付きで並ぶ。
    #[tokio::test]
    async fn lists_credentials_that_were_never_used() {
        let base = serve_with_default_ns(
            r#"
[credentials.bedrock]
type = "claude_bedrock"
url = "https://bedrock.invalid/anthropic"

[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]
"#,
        )
        .await;

        let report = get(&base, "/llm-gateway/usage").await;
        assert!(report["generated_at"].as_i64().unwrap() > 1_700_000_000);
        assert!(report.get("probe").is_none(), "既定では投げない");

        let bedrock = entry(&report, "bedrock");
        assert_eq!(bedrock["type"], "claude_bedrock");
        assert_eq!(bedrock["support"], "not_applicable");
        assert!(
            bedrock["note"].as_str().unwrap().contains("AWS"),
            "対象外の理由が読める: {bedrock}"
        );

        let cpa = entry(&report, "cpa");
        assert_eq!(cpa["support"], "upstream_dependent");
        assert!(cpa["note"].as_str().unwrap().contains("転送先"));
        assert!(cpa.get("snapshot").is_none(), "未観測に中身は無い");
    }

    /// 転送のついでに読んだ値が出る。追加の API コールは要らない。
    #[tokio::test]
    async fn a_forwarded_response_fills_the_snapshot() {
        // 同じ応答を一覧の問い合わせにも返す。転送の試験に要るのは
        // ヘッダだけなので、本文はモデル一覧の形にしておく。
        let upstream = fake_upstream(|| {
            (
                200,
                r#"{"data":[{"id":"m","created_at":"2026-07-24T00:00:00Z"}]}"#.to_owned(),
                unified_headers(),
            )
        })
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.claude-personal]
type = "claude_oauth"
url = "{upstream}"

[[ns.default.routing]]
models = ["m"]
credentials = ["claude-personal"]
"#
        ))
        .await;

        let before = get(&base, "/llm-gateway/usage").await;
        assert_eq!(entry(&before, "claude-personal")["support"], "unobserved");

        authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&json!({"model": "m", "max_tokens": 8, "messages": []})),
        )
        .send()
        .await
        .unwrap();

        let after = get(&base, "/llm-gateway/usage").await;
        let c = entry(&after, "claude-personal");
        assert_eq!(c["support"], "observed");

        let s = &c["snapshot"];
        assert_eq!(s["5h"]["utilization"], 0.71);
        assert_eq!(s["5h"]["status"], "allowed");
        assert_eq!(s["5h"]["reset"], 1_785_344_400_i64);
        assert_eq!(
            s["5h"]["reset_iso"], "2026-07-29T17:00:00Z",
            "Unix 秒と ISO の両方を出す"
        );
        assert_eq!(s["7d"]["utilization"], 0.3);
        assert_eq!(s["overage"]["disabled_reason"], "out_of_credits");
        assert!(
            s["observed_at"].as_i64().unwrap() > 1_700_000_000,
            "いつ観測したかが分かる: {s}"
        );
    }

    /// `?refresh=true` のときだけ投げに行く。消費した分を出力に残す。
    #[tokio::test]
    async fn refresh_probes_and_reports_what_it_spent() {
        let upstream = fake_upstream(|| {
            (
                200,
                r#"{"type":"message","usage":{"input_tokens":8,"output_tokens":1}}"#.to_owned(),
                unified_headers(),
            )
        })
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.claude-personal]
type = "claude_oauth"
url = "{upstream}"
"#
        ))
        .await;

        let report = get(&base, "/llm-gateway/usage?refresh=true").await;
        let probe = &report["probe"];
        assert_eq!(probe["requests"], 1);
        assert_eq!(probe["model"], "claude-haiku-4-5-20251001");
        assert_eq!(probe["input_tokens"], 8, "確認そのものが消費した分");
        assert_eq!(probe["output_tokens"], 1);

        let c = entry(&report, "claude-personal");
        assert_eq!(c["support"], "observed", "投げた結果が反映される");
        assert!(c.get("probe_error").is_none());
    }

    /// 1 つ失敗しても、他の credential は返る。
    #[tokio::test]
    async fn a_failing_probe_does_not_empty_the_list() {
        let upstream = fake_upstream(|| {
            (
                200,
                r#"{"usage":{"input_tokens":8,"output_tokens":1}}"#.to_owned(),
                unified_headers(),
            )
        })
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.alive]
type = "claude_oauth"
url = "{upstream}"

[credentials.nowhere]
type = "claude_oauth"
url = "http://127.0.0.1:9"
"#
        ))
        .await;

        let report = get(&base, "/llm-gateway/usage?refresh=true").await;
        assert_eq!(report["probe"]["requests"], 2);

        let alive = entry(&report, "alive");
        assert_eq!(alive["support"], "observed");
        assert!(alive.get("probe_error").is_none());

        let dead = entry(&report, "nowhere");
        assert_eq!(dead["support"], "unobserved");
        assert!(
            !dead["probe_error"].as_str().unwrap().is_empty(),
            "何が起きたか書いてある: {dead}"
        );
    }

    /// 上限に当たった応答からも読む (むしろそのときこそ見たい)。
    #[tokio::test]
    async fn a_rate_limited_probe_still_yields_a_snapshot() {
        let upstream = fake_upstream(|| {
            let mut headers = unified_headers();
            headers.push((
                "anthropic-ratelimit-unified-5h-status".into(),
                "rejected".into(),
            ));
            (429, r#"{"error":"rate_limit"}"#.to_owned(), headers)
        })
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[credentials.busy]
type = "claude_oauth"
url = "{upstream}"
"#
        ))
        .await;

        let c = entry(&get(&base, "/llm-gateway/usage?refresh=true").await, "busy").clone();
        assert_eq!(c["support"], "observed", "429 でもヘッダは読む");
        assert!(
            c["probe_error"].as_str().unwrap().contains("429"),
            "失敗したことも隠さない: {c}"
        );
    }

    /// 死活監視と同じで、トークンは要らない (DR-0007)。
    #[tokio::test]
    async fn usage_needs_no_token() {
        let base = serve_with_default_ns(
            r#"
[credentials.cpa]
type = "relay"
url = "http://127.0.0.1:9"
models = ["m"]
"#,
        )
        .await;

        let resp = reqwest::get(format!("{base}/llm-gateway/usage"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    /// namespace 付きでは生やさない (gateway 自身の機能なので)。
    #[tokio::test]
    async fn usage_is_not_namespaced() {
        let base = serve_with_default_ns("").await;
        let resp = reqwest::get(format!("{base}/ns-default/llm-gateway/usage"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    /// 値を見る。`refresh=false` を「付いている」と読むと、消費しない側を
    /// 選んだ相手に実リクエストを投げることになる。
    #[test]
    fn refresh_is_opt_in() {
        assert!(wants_refresh(Some("refresh=true")));
        assert!(wants_refresh(Some("refresh=1")));
        assert!(wants_refresh(Some("x=1&refresh=true")));

        for off in [None, Some(""), Some("refresh=false"), Some("refresh=0")] {
            assert!(!wants_refresh(off), "{off:?}");
        }
        assert!(!wants_refresh(Some("refresh")), "値なしは付いていない扱い");
    }
}

#[cfg(test)]
mod e2e_namespace_tests {
    use super::tests::{TOKEN, authed, serve};
    use serde_json::Value;

    /// 2 つの namespace を持つ設定。見えるモデルが違う。
    ///
    /// `tokenless` は `auth_token` を書き忘れた namespace。誰も通れない。
    const TWO_NS: &str = r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5", "claude-fable-5", "gpt-5.6-sol"]

[ns.personal]
auth_token = "test-token"
[ns.personal.filter]
exclude = ["gpt-*"]
[ns.personal.aliases]
opus = "claude-opus-*"

[ns.work]
auth_token = "test-token"
[ns.work.filter]
exclude = ["claude-fable-*"]

[ns.locked]
auth_token = "secret-token"

[ns.tokenless]
"#;

    async fn ids(base: &str, path: &str) -> Vec<String> {
        let body: Value = authed(reqwest::Client::new().get(format!("{base}{path}")))
            .send()
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
        let resp = authed(reqwest::Client::new().get(format!("{base}/ns-nope/v1/models")))
            .send()
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

    /// `auth_token` を書いていない namespace は誰も通れない (fail-closed)。
    ///
    /// 書き忘れが「誰でも通れる穴」になっていた。設定ファイルの外 (前段に
    /// 公開経路が生えた) で前提が崩れても、書き忘れが穴にならないようにする
    /// (DR-0006)。
    #[tokio::test]
    async fn namespace_without_token_lets_nobody_in() {
        let base = serve(TWO_NS).await;
        let client = reqwest::Client::new();

        let bare = client
            .get(format!("{base}/ns-tokenless/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(bare.status(), 401, "名乗らない相手も通さない");

        for value in ["Bearer anything", TOKEN, "secret-token"] {
            let resp = client
                .get(format!("{base}/ns-tokenless/v1/models"))
                .header("authorization", value)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401, "{value} でも通らない");
        }
    }

    /// 通さない理由で文言が違う。
    ///
    /// 同じ文言だと、正しいトークンを送っている利用者が「合っているはずなのに」
    /// で止まり、原因が gateway 側の設定漏れだと気づけない。
    #[tokio::test]
    async fn denial_message_tells_which_problem_it_is() {
        let base = serve(TWO_NS).await;
        let client = reqwest::Client::new();

        let wrong: Value = client
            .get(format!("{base}/ns-locked/v1/models"))
            .header("authorization", "Bearer nope")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let wrong = wrong["error"]["message"].as_str().unwrap().to_owned();
        assert!(wrong.contains("トークンが違います"), "{wrong}");

        let unset: Value = client
            .get(format!("{base}/ns-tokenless/v1/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let unset = unset["error"]["message"].as_str().unwrap().to_owned();
        assert!(
            unset.contains("auth_token"),
            "設定側の問題だと分かる文言: {unset}"
        );
        assert!(
            unset.contains("tokenless"),
            "どの namespace か分かる: {unset}"
        );
        assert_ne!(wrong, unset, "2 つの理由を同じ文言にしない");
    }

    /// `[ns.default]` を書かなければ `/v1/...` は 404。
    ///
    /// 既定 namespace を特別扱いしないので、`/v1/...` は `/ns-default/...`
    /// と同じ扱いになる。「名前を明示したリクエストしか受けない」構成は
    /// `[ns.default]` を書かないことで実現できる (DR-0006)。
    #[tokio::test]
    async fn plain_path_is_404_without_a_default_namespace() {
        let base = serve(TWO_NS).await;

        let client = reqwest::Client::new();
        let listing = authed(client.get(format!("{base}/v1/models")))
            .send()
            .await
            .unwrap();
        assert_eq!(listing.status(), 404, "/v1/models");

        let sending = authed(client.post(format!("{base}/v1/messages")))
            .json(&serde_json::json!({"model": "claude-opus-5", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(sending.status(), 404, "/v1/messages");

        let body: Value = authed(reqwest::Client::new().get(format!("{base}/v1/models")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains("default"), "どの namespace が無いのか: {msg}");
    }

    /// `[ns.default]` を書けば `/v1/...` が使える。
    #[tokio::test]
    async fn plain_path_works_once_the_default_namespace_exists() {
        let base = super::tests::serve_with_default_ns(
            r#"
[credentials.a]
type = "relay"
url = "http://127.0.0.1:9"
models = ["claude-opus-5"]
"#,
        )
        .await;

        assert_eq!(
            ids(&base, "/v1/models").await,
            ids(&base, "/ns-default/v1/models").await,
            "`/v1/...` は `/ns-default/...` と同じもの"
        );
        assert!(
            ids(&base, "/v1/models")
                .await
                .contains(&"claude-opus-5".to_owned())
        );
    }

    /// 死活監視の口は、認証を設定した namespace があっても通る。
    ///
    /// ここが認証に巻き込まれると、前段の監視が落ちて upstream ごと
    /// 切り離される。業務のエンドポイントと分けてある理由がこれ。
    #[tokio::test]
    async fn healthz_needs_no_token() {
        let base = serve(TWO_NS).await;
        let resp = reqwest::get(format!("{base}/llm-gateway/healthz"))
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    /// 死活監視の口は namespace を取らない。
    #[tokio::test]
    async fn healthz_is_not_namespaced() {
        let base = serve(TWO_NS).await;
        let resp = reqwest::get(format!("{base}/ns-locked/llm-gateway/healthz"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "namespace 付きでは生やさない");
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
