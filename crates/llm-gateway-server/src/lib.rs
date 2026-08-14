//! HTTP 層。Anthropic Messages API を話す口を生やす。
//!
//! 実運用のログから、クライアントが叩くのは 3 つと分かっている:
//! `POST /v1/messages` / `POST /v1/messages/count_tokens` / `GET /v1/models`。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::{Instrument as _, error, warn};

use llm_gateway::config::{Authorization, Namespace};
use llm_gateway::credential::time::now_unix;
use llm_gateway::credential::{CredentialId, Persistence};
use llm_gateway::{Error, Gateway, exchange};
use tokio::sync::broadcast::error::RecvError;

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
        .route("/llm-gateway/stats", get(stats))
        .route("/llm-gateway/events", get(events))
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
    json_utf8(Json(gateway.usage_report(refresh).await))
}

/// 自前の JSON 応答に文字コードを明示する。
///
/// JSON は仕様上 UTF-8 だが、`charset` の無い `application/json` を
/// レガシーな文字コードで描画するブラウザがあり、日本語の note が化ける。
/// 触るのは gateway 自身が組み立てる応答だけで、upstream の透過分には
/// 手を入れない。
fn json_utf8(body: impl IntoResponse) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

/// 転送のたびに起きたことを流し続ける (DR-0012)。
///
/// SSE で返す。届くのは**繋いだ後**に起きた分だけで、過去には遡らない。
/// 見ている側が遅れて取りこぼしても、こちらは詰まらない (落として先へ進む)。
///
/// 認証は usage / stats と同じ扱い (掛けない)。手前の tailnet の境界を信頼
/// する。流すのは「いつ・どの会話が・どの経路に当たったか」までで、本文も
/// トークン数も載せない。
///
/// どの生成元からでも読めるようにしておく。主な相手はサーバ同士で話す
/// ccmsg だが、様子を見るのにブラウザから直接開けると早い。認証を持たない
/// 口なので、生成元で絞っても守れるものが増えない。
async fn events<P: Persistence + 'static>(
    State(gateway): State<Arc<Gateway<P>>>,
) -> impl IntoResponse {
    let watching = gateway.events().subscribe();

    let stream = futures_util::stream::unfold(watching, |mut watching| async move {
        loop {
            match watching.recv().await {
                Ok(event) => {
                    return Some((
                        Ok::<SseEvent, std::convert::Infallible>(sse_line(&event)),
                        watching,
                    ));
                }
                // 追いつけなかった分は諦めて先へ進む。5 分の残りを数える相手に、
                // 遅れて届いた開始時刻を渡しても使い道がない。
                Err(RecvError::Lagged(missed)) => {
                    warn!(missed, "イベントを配りきれませんでした");
                }
                // 流す側が畳まれた。gateway が終わるとき以外は起きない。
                Err(RecvError::Closed) => return None,
            }
        }
    });

    // 何も起きない時間が続いても、経路上の中継に切られないよう合図を送る。
    let sse =
        Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(20)));
    ([(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], sse)
}

/// 1 件を SSE の 1 通にする。
///
/// 載せるのは自分で組み立てた構造体だけなので、JSON にできない事態は
/// 起きない。それでも配信を止めないよう、万一のときは空の 1 通を送る。
fn sse_line(event: &llm_gateway::events::Event) -> SseEvent {
    SseEvent::default()
        .event("request")
        .json_data(event)
        .unwrap_or_else(|e| {
            error!(%e, "イベントを JSON にできません");
            SseEvent::default().comment("unserializable")
        })
}

/// 使用量の日次集計を返す (DR-0011)。
///
/// 認証は usage / healthz と同じ扱い (掛けない)。出すのはトークン数だけで、
/// 何を書いたかは残していない。
///
/// `?days=N` で直近 N 日に絞る。既定を 7 日にするのは、全期間を返すと日が
/// 経つほど応答が伸びるため。`days=0` なら全部。
async fn stats<P: Persistence + 'static>(
    State(gateway): State<Arc<Gateway<P>>>,
    request: Request,
) -> Response {
    let days = match requested_days(request.uri().query()) {
        Ok(days) => days,
        Err(message) => return client_error("stats", StatusCode::BAD_REQUEST, &message),
    };
    json_utf8(Json(gateway.stats_report(days, now_unix())))
}

/// 既定で見せる日数。
const DEFAULT_DAYS: usize = 7;

/// `?days=N` を読む。付いていなければ既定。
///
/// 上限で抑えるのは、日数を秒に直す掛け算が桁あふれするため。抑えないと
/// 絞り込みの起点が未来に回って全部消える。
fn requested_days(query: Option<&str>) -> Result<usize, String> {
    let Some(raw) = query.and_then(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == "days")
            .map(|(_, v)| v)
    }) else {
        return Ok(DEFAULT_DAYS);
    };
    raw.parse::<usize>()
        .map(|days| days.min(llm_gateway::stats::MAX_DAYS))
        .map_err(|_| format!("`days` must be a whole number, got `{raw}`"))
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

/// namespace の方針を Messages 形式の正規形へ反映する。
fn apply_thinking_display(body: &mut Value, display: Option<llm_gateway::config::ThinkingDisplay>) {
    let Some(display) = display else {
        return;
    };
    let Some(body) = body.as_object_mut() else {
        return;
    };

    match body.get_mut("thinking") {
        Some(Value::Object(thinking)) => {
            thinking.insert(
                "display".to_owned(),
                Value::String(display.as_str().to_owned()),
            );
        }
        Some(_) => {}
        None => {
            body.insert(
                "thinking".to_owned(),
                json!({"type": "adaptive", "display": display.as_str()}),
            );
        }
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
    let span = exchange::request_span();
    // 断るときのログにも載せるので、経路が決まる前に取り出しておく。
    let ns_name = namespace_of(uri.path()).to_owned();

    // Design rationale: 応答は流すのに、要求は全部読んでから渡す。
    //
    // 要求の本文は 1 回で終わらない。どのモデルかを読まないと経路が決まらず
    // (`Gateway::forward` の `model_of`)、決まった経路は上から順に試すので、
    // 断られるたびに同じ本文を次の upstream へ送り直す。beta フラグを落として
    // の送り直し (DR-0003) も同じ本文をもう 1 度使う。しかも送る内容は経路
    // ごとに違う — 短い名前の解決や Bedrock の名前空間付与で `model` を
    // 書き換えるため、経路の数だけ別の本文ができる。読み流しの本文は 1 度
    // しか読めないので、この作りとは両立しない。
    //
    // 部分的に読んで `model` だけ差し替える手もあるが、JSON のキーの順は
    // 決まっておらず `model` が長い `messages` の後ろに来ることもある。
    // 結局そこまで抱えることになるうえ、送り直しのために抱え続ける必要は
    // 変わらない。上限 (`MAX_BODY`) までのメモリを見込むのはその代償。
    let receiving = Instant::now();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return span.in_scope(|| {
                client_error(
                    &ns_name,
                    StatusCode::BAD_REQUEST,
                    &format!("本文を読めません: {e}"),
                )
            });
        }
    };
    // 大きさは受け取った実バイト数で数える。Content-Length は手前の
    // プロキシが chunked で渡してくると付かないが、こちらは必ず取れる。
    exchange::record_request_body(&span, bytes.len(), receiving.elapsed());

    let mut json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return span.in_scope(|| {
                client_error(
                    &ns_name,
                    StatusCode::BAD_REQUEST,
                    &format!("JSON として読めません: {e}"),
                )
            });
        }
    };

    let headers = collect_headers(&parts.headers);
    let Some(ns) = gateway.namespace(&ns_name) else {
        return span.in_scope(|| unknown_namespace(&ns_name, &gateway.namespace_names()));
    };
    if let Some(denied) = span.in_scope(|| rejection(ns, &ns_name, &parts.headers)) {
        return denied;
    }
    apply_thinking_display(&mut json, ns.thinking_display);

    let path = upstream_path(uri.path()).to_owned();
    let query = uri.query().map(str::to_owned);

    match gateway
        .forward(ns, &ns_name, &path, query.as_deref(), json, headers)
        .instrument(span.clone())
        .await
    {
        Ok(forwarded) => {
            let upstream = forwarded.response;
            let mut resp = Response::builder().status(upstream.status);
            for (name, value) in upstream.headers.iter() {
                resp = resp.header(name, value);
            }

            // 本文は読まずに流す。SSE はここを通り抜けるだけ。包むのは
            // 終端 (流し切った / 途切れた / 中断された) を残すためと、
            // 使用量を覗くため — 応答の本文にしか載らないので、中継の内側で
            // 覗く (DR-0011)。覗くだけで、流れるバイト列は変わらない。読む役は
            // 答えた provider が作ったものが載っている (DR-0014 §4)。
            resp.body(Body::from_stream(exchange::observe(
                upstream.body,
                forwarded.usage,
                Arc::clone(gateway.stats()),
                now_unix(),
                forwarded.credential.as_ref().map(CredentialId::as_str),
                &forwarded.model,
                span.clone(),
            )))
            .unwrap_or_else(|e| {
                span.in_scope(|| {
                    client_error(
                        &ns_name,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("could not build the response: {e}"),
                    )
                })
            })
        }
        Err(e) => span.in_scope(|| error_response(&ns_name, &e)),
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
    json_utf8(Json(json!({"object": "list", "data": data})))
}

fn unknown_namespace(name: &str, known: &[&str]) -> Response {
    client_error(
        name,
        StatusCode::NOT_FOUND,
        &format!(
            "namespace `{name}` は設定されていません。使えるのは: {}",
            known.join(", ")
        ),
    )
}

/// トークンを検査して、通さないなら返す応答を作る。
///
/// `auth_token` を書いていない namespace は検査せずに通す (DR-0006)。手前で
/// 境界を引く運用では、ここで二重に認証を求める意味がない。
fn rejection(ns: &Namespace, name: &str, headers: &HeaderMap) -> Option<Response> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match ns.authorize(presented) {
        Authorization::Accepted | Authorization::Open => None,
        Authorization::WrongToken => Some(refused(
            name,
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            &format!("namespace `{name}` のトークンが違います"),
        )),
    }
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
fn error_response(ns: &str, e: &Error) -> Response {
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

    refused(ns, status, kind, &message)
}

/// 入口で断るときの応答。`invalid_request_error` で返す。
fn client_error(ns: &str, status: StatusCode, message: &str) -> Response {
    refused(ns, status, "invalid_request_error", message)
}

/// クライアントへ返す失敗を組み立てて、断ったことを 1 行残す。
///
/// ログをここに置くのは、経路を試す前に確定する失敗が転送のログに何も
/// 残さないから。upstream へ行かないので経路切替も全滅も記録されず、404 や
/// 401 の原因は再現手順を踏むまで分からない (実際に、namespace の filter で
/// 外したモデルへの 404 をログから追えなかった)。
fn refused(ns: &str, status: StatusCode, kind: &str, message: &str) -> Response {
    let status_code = status.as_u16();
    if status.is_server_error() {
        error!(%ns, status = status_code, %message, "リクエストを処理できません");
    } else {
        warn!(%ns, status = status_code, %message, "リクエストを断りました");
    }

    json_utf8((
        status,
        Json(json!({
            "type": "error",
            "error": {"type": kind, "message": message},
        })),
    ))
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
        /// 置き場を共有する相手がいないので、締め出すものが無い。
        type Guard = ();

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
        fn lock(&self, _id: &CredentialId) -> llm_gateway::Result<Self::Guard> {
            Ok(())
        }
        /// 版を持たない。中身が動かないので控えを疑う理由が無い。
        fn version(&self, _id: &CredentialId) -> Option<u64> {
            None
        }
    }

    /// 受け取ったリクエストを覚えておく試験用 upstream。
    /// 受け取った要求をそのまま覚えておく upstream。
    ///
    /// 何が漏れて**いない**かを見るのに使う。「送っていないはず」を確かめるには、
    /// 実際に届いたバイト列を読む以外にない。
    pub(crate) async fn recording_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut buf = vec![0u8; 65536];
                    let read = sock.read(&mut buf).await.unwrap_or(0);

                    // 一覧の問い合わせ (discovery) にも答える。答えないと、
                    // モデルを聞きに行く型の credential で経路が立たない。
                    let seen_request = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let body = if seen_request.starts_with("GET /v1/models") {
                        r#"{"data":[{"id":"m","created_at":"2026-07-24T00:00:00Z"}]}"#
                    } else {
                        r#"{"type":"message","content":[]}"#
                    };
                    recorder.lock().unwrap().push(seen_request);

                    let head = format!(
                        "HTTP/1.1 200 X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        (format!("http://{addr}"), seen)
    }

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
    pub(crate) fn captured_logs() -> &'static Mutex<Vec<u8>> {
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
    /// 認証の有無そのものを見る試験と分けるため、認証以外を
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
        serve_inner(config_toml, None).await
    }

    /// 前回落とした分を読み戻してから立てる (再起動に相当)。
    ///
    /// 読み戻しは置き場を読むので、`[stats] dir` を試験用の置き場に向けた
    /// 設定でだけ使う。既定のままだと利用者の実データを読む。
    pub(crate) async fn serve_restored(config_toml: &str, now: i64) -> String {
        serve_inner(config_toml, Some(now)).await
    }

    async fn serve_inner(config_toml: &str, restore_at: Option<i64>) -> String {
        let config: Config = toml::from_str(config_toml).unwrap();
        config.validate().unwrap();
        let gateway = Arc::new(Gateway::new(&config, StaticStore).unwrap());
        gateway.refresh_models().await;
        if let Some(now) = restore_at {
            gateway.restore(now).await;
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(gateway)).await;
        });
        format!("http://{addr}")
    }

    pub(crate) fn request_body() -> Value {
        json!({
            "model": "m",
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    /// 転送のたびに、見ている人へ 1 通流れる。
    ///
    /// 5 分の残りを数えるのはこの時刻から。SSE の形 (event / data の 2 行) と
    /// 中身の欄が、見る側の契約になる (DR-0012)。
    #[tokio::test]
    async fn a_forward_is_announced_to_watchers() {
        let upstream = fake_upstream(|| {
            (
                200,
                r#"{"type":"message","content":[]}"#.to_owned(),
                vec![("content-type".to_owned(), "application/json".to_owned())],
            )
        })
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#
        ))
        .await;

        // 先に見始める。応答のヘッダが返った時点で、もう見る側に回っている。
        let mut watching = reqwest::Client::new()
            .get(format!("{base}/llm-gateway/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(watching.status(), 200);
        assert_eq!(
            watching.headers().get("content-type").unwrap(),
            "text/event-stream",
            "SSE として読める形で返す"
        );
        assert_eq!(
            watching
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "*",
            "様子を見るのにブラウザから直接開ける"
        );

        // system を積んだ本文。系列の識別子はこの先頭ブロックから作る。
        let mut body = request_body();
        body["system"] = json!([
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.0.1"},
            {"type": "text", "text": "gitStatus: branch main, clean"},
        ]);

        let forwarded = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .header("X-Claude-Code-Session-Id", "s-1")
                .json(&body),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(forwarded.status(), 200);

        let chunk = watching.chunk().await.unwrap().expect("1 通届く");
        let text = String::from_utf8(chunk.to_vec()).unwrap();

        let (kind, data) = text.trim_end().split_once('\n').expect("2 行");
        assert_eq!(kind, "event: request");
        let event: Value =
            serde_json::from_str(data.strip_prefix("data: ").expect("data 行")).unwrap();

        assert_eq!(event["session_id"], "s-1", "会話が分かる");
        assert_eq!(event["ns"], "default");
        assert_eq!(event["model"], "m");
        assert_eq!(event["credential"], "a");
        assert_eq!(event["status"], 200);
        assert!(event["ts"].as_i64().unwrap() > 0);
        assert!(event["ts_iso"].as_str().unwrap().ends_with('Z'));

        let series = event["prefix"].as_str().expect("系列が分かる");
        assert_eq!(series.len(), 8, "短い 16 進 1 つ: {series}");
        assert!(series.chars().all(|c| c.is_ascii_hexdigit()), "{series}");
    }

    /// 会話を名乗らないクライアント (curl 等) の分も流す。
    #[tokio::test]
    async fn a_request_without_a_session_is_still_announced() {
        let upstream = fake_upstream(|| {
            (
                429,
                r#"{"type":"error","error":{"message":"nope"}}"#.to_owned(),
                vec![("content-type".to_owned(), "application/json".to_owned())],
            )
        })
        .await;

        let base = serve_with_default_ns(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#
        ))
        .await;

        let mut watching = reqwest::Client::new()
            .get(format!("{base}/llm-gateway/events"))
            .send()
            .await
            .unwrap();

        let refused = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&request_body()),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(refused.status(), 429);

        let chunk = watching.chunk().await.unwrap().expect("断られた分も流れる");
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("data 行");
        let event: Value = serde_json::from_str(data).unwrap();

        assert!(event["session_id"].is_null(), "名乗らなければ null");
        assert_eq!(event["status"], 429, "断られたことも知らせ");
        assert!(
            event.get("prefix").is_none(),
            "system を積んでいないので系列も分からない"
        );
    }

    /// 設定された namespace では、クライアントが指定した思考形式を保ちつつ
    /// 表示方法だけを namespace の方針で上書きする。
    #[test]
    fn configured_display_overrides_existing_thinking_object() {
        let mut body = json!({
            "model": "m",
            "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 1024, "display": "omitted"},
        });

        apply_thinking_display(
            &mut body,
            Some(llm_gateway::config::ThinkingDisplay::Summarized),
        );

        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024, "display": "summarized"}),
            "display 以外のクライアント指定は保持する"
        );
    }

    /// 思考指定が無い request に表示方針を適用する場合は、5 系で推奨される
    /// adaptive thinking を補って Messages 正規形を完成させる。
    #[test]
    fn configured_display_injects_adaptive_thinking_when_absent() {
        for (display, expected) in [
            (
                llm_gateway::config::ThinkingDisplay::Summarized,
                "summarized",
            ),
            (llm_gateway::config::ThinkingDisplay::Omitted, "omitted"),
        ] {
            let mut body = json!({"model": "m", "messages": []});

            apply_thinking_display(&mut body, Some(display));

            assert_eq!(
                body["thinking"],
                json!({"type": "adaptive", "display": expected}),
                "{expected}"
            );
        }
    }

    /// namespace が表示方針を持たなければ、透過 proxy の既定として request の
    /// JSON 値を一切変えない。thinking の有無の両方で同じ契約になる。
    #[test]
    fn unconfigured_display_leaves_request_unchanged() {
        for original in [
            json!({"model": "m", "messages": []}),
            json!({
                "model": "m",
                "messages": [],
                "thinking": {"type": "enabled", "display": "omitted"},
            }),
        ] {
            let mut body = original.clone();

            apply_thinking_display(&mut body, None);

            assert_eq!(body, original);
        }
    }

    /// namespace の表示方針は、認証と namespace 解決を通った実際の Messages
    /// ingress で適用し、provider 変換へ渡す正規形に載せる。
    #[tokio::test]
    async fn configured_display_reaches_upstream_through_messages_ingress() {
        let (upstream, seen) = recording_upstream().await;
        let base = serve(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[ns.default]
auth_token = "{TOKEN}"
thinking_display = "summarized"

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#
        ))
        .await;

        let response = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&request_body()),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 200);

        let requests = seen.lock().unwrap();
        let request = requests
            .iter()
            .find(|request| request.starts_with("POST /v1/messages"))
            .expect("Messages request が upstream に届く");
        let (_, raw_body) = request
            .split_once("\r\n\r\n")
            .expect("HTTP header と body が分かれる");
        let body: Value = serde_json::from_str(raw_body).expect("upstream body は JSON");
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
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
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
    ///
    /// 断った記録も残す。upstream へ行かないので転送のログには何も出ず、
    /// 残さないと「なぜ 404 になったか」を再現手順を踏むまで追えない。
    #[tokio::test]
    async fn unknown_model_yields_404() {
        let logs = captured_logs();
        let base = serve_with_default_ns(
            r#"
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m", "claude-opus-5", "claude-fable-5"]

[[ns.default.routing]]
models = ["known"]
routes = ["a"]
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

        let text = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
        let refused = text
            .lines()
            .find(|l| l.contains("リクエストを断りました") && l.contains("no-such-model"))
            .unwrap_or_else(|| panic!("断った記録が無い:\n{text}"));
        assert!(
            refused.contains("ns=default"),
            "どの namespace か: {refused}"
        );
        assert!(refused.contains("status=404"), "何を返したか: {refused}");
        assert!(refused.contains("req="), "どの要求か: {refused}");
    }

    /// 経路が全滅したら 503。どこで何が起きたかを返す。
    #[tokio::test]
    async fn all_routes_down_yields_503_with_detail() {
        let down = fake_upstream(|| (503, "{}".to_owned(), vec![])).await;
        let base = serve_with_default_ns(&format!(
            r#"
[routes.a]
provider = "anthropic"
url = "{down}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m", "claude-opus-5", "claude-fable-5"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
[routes.a]
provider = "anthropic"
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
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
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
    use super::tests::{authed, fake_upstream, serve_restored, serve_with_default_ns};
    use super::*;
    use serde_json::json;

    /// 実測された unified ヘッダ (DR-0007 の表)。
    fn unified_headers() -> Vec<(String, String)> {
        vec![
            // 本文の読み手 (usage の抽出) は content-type で決まる。実機の応答は
            // 必ず載せてくるので、枠ヘッダの見本にも一緒に入れておく。
            ("content-type".into(), "application/json".into()),
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

    /// 前の起動で観測した分が、再起動しても一覧に出る (DR-0007)。
    ///
    /// 観測は通りすがりでしか起きないので、消えると次にその credential を
    /// 使うまで何も見えない。取得時刻も一緒に戻るので、古さは読み手が判断できる。
    #[tokio::test]
    async fn a_snapshot_observed_before_the_restart_is_still_listed() {
        use llm_gateway::credential::CredentialId;
        use llm_gateway::egress::Headers;
        use llm_gateway::preset::anthropic::AnthropicMetering;
        use llm_gateway::provider::Metering as _;
        use llm_gateway::quota::QuotaStore;

        let dir = tempfile::tempdir().unwrap();
        let listen = "127.0.0.1:11300";
        // 3 時間前に観測して落とした、前の起動の分。
        let observed_at = 1_785_326_400 - 3 * 3600;
        {
            let snapshot = AnthropicMetering
                .quota_snapshot(&Headers::new(unified_headers()), observed_at)
                .expect("枠が載っている");
            let usage = QuotaStore::new(dir.path(), listen);
            usage
                .observe(&CredentialId::new("claude-personal"), snapshot)
                .await;
            usage.save().await.unwrap();
        }

        let base = serve_restored(
            &format!(
                r#"
[server]
listen = "{listen}"

[stats]
dir = "{}"

[credentials.claude-personal]
type = "claude_oauth"

[routes.claude-personal]
provider = "anthropic"
credential = "claude-personal"
url = "https://upstream.invalid"
"#,
                dir.path().display()
            ),
            observed_at,
        )
        .await;

        let c = entry(&get(&base, "/llm-gateway/usage").await, "claude-personal").clone();
        assert_eq!(c["support"], "observed", "未観測に戻さない: {c}");
        assert_eq!(c["snapshot"]["5h"]["utilization"], 0.71);
        assert_eq!(
            c["snapshot"]["observed_at"], observed_at,
            "取得時刻は観測した当時のまま (読み戻した時刻に付け替えない): {c}"
        );
    }

    /// 使ったことのない credential も、名前と理由付きで並ぶ。
    #[tokio::test]
    async fn lists_credentials_that_were_never_used() {
        let base = serve_with_default_ns(
            r#"
[credentials.bedrock]
type = "bedrock_api_key"

[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock.invalid/anthropic"

[routes.cpa]
provider = "anthropic"
url = "http://127.0.0.1:9"
models = ["m"]
"#,
        )
        .await;

        let report = get(&base, "/llm-gateway/usage").await;
        assert!(report["generated_at"].as_i64().unwrap() > 1_700_000_000);
        assert!(report.get("probe").is_none(), "既定では投げない");

        let bedrock = entry(&report, "bedrock");
        assert_eq!(bedrock["type"], "bedrock_api_key");
        assert_eq!(bedrock["support"], "not_applicable");
        assert!(
            bedrock.get("note").is_none(),
            "理由の文章は出さない: {bedrock}"
        );

        let cpa = entry(&report, "cpa");
        assert_eq!(cpa["support"], "upstream_dependent");
        assert!(cpa.get("note").is_none());
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

[routes.claude-personal]
provider = "anthropic"
credential = "claude-personal"
url = "{upstream}"

[[ns.default.routing]]
models = ["m"]
routes = ["claude-personal"]
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

[routes.claude-personal]
provider = "anthropic"
credential = "claude-personal"
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

[routes.alive]
provider = "anthropic"
credential = "alive"
url = "{upstream}"

[credentials.nowhere]
type = "claude_oauth"

[routes.nowhere]
provider = "anthropic"
credential = "nowhere"
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

[routes.busy]
provider = "anthropic"
credential = "busy"
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
[routes.cpa]
provider = "anthropic"
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
    use super::tests::{TOKEN, authed, captured_logs, recording_upstream, request_body, serve};
    use serde_json::Value;

    /// 2 つの namespace を持つ設定。見えるモデルが違う。
    ///
    /// `tokenless` は `auth_token` を書かない namespace。誰でも通れる。
    const TWO_NS: &str = r#"
[routes.a]
provider = "anthropic"
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
    ///
    /// 通さなかったことは記録に残す。どの namespace で弾いたか分からないと、
    /// 設定の書き間違いとトークンの誤りを切り分けられない。
    #[tokio::test]
    async fn namespace_token_is_checked() {
        let logs = captured_logs();
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

        let text = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
        let refused = text
            .lines()
            .find(|l| l.contains("リクエストを断りました") && l.contains("ns=locked"))
            .unwrap_or_else(|| panic!("断った記録が無い:\n{text}"));
        assert!(refused.contains("status=401"), "何を返したか: {refused}");
        assert!(
            !refused.contains("secret-token"),
            "トークンは書かない: {refused}"
        );

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

    /// `auth_token` を書いていない namespace は、名乗り方に関わらず通す。
    ///
    /// 境界は手前 (tailnet / リバースプロキシ) で引く。クライアントに
    /// トークンを持たせないためでもある — Claude Code は
    /// `ANTHROPIC_AUTH_TOKEN` があるとサブスクとしての振る舞いをやめる
    /// (DR-0006)。
    #[tokio::test]
    async fn namespace_without_token_lets_everyone_in() {
        let base = serve(TWO_NS).await;
        let client = reqwest::Client::new();

        let bare = client
            .get(format!("{base}/ns-tokenless/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(bare.status(), 200, "名乗らない相手も通す");

        for value in ["Bearer anything", TOKEN, "secret-token", ""] {
            let resp = client
                .get(format!("{base}/ns-tokenless/v1/models"))
                .header("authorization", value)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{value:?} でも通す");
        }
    }

    /// トークンを求める namespace は、間違いをそのまま断る。
    ///
    /// 隣に認証なしの namespace があっても緩まない。
    #[tokio::test]
    async fn a_locked_namespace_still_checks_its_token() {
        let base = serve(TWO_NS).await;
        let client = reqwest::Client::new();

        let refused = client
            .get(format!("{base}/ns-locked/v1/models"))
            .header("authorization", "Bearer nope")
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 401);

        let message: Value = refused.json().await.unwrap();
        let message = message["error"]["message"].as_str().unwrap();
        assert!(message.contains("トークンが違います"), "{message}");
        assert!(
            message.contains("locked"),
            "どの namespace か分かる: {message}"
        );
    }

    /// クライアントが名乗った認証情報を upstream へ渡さない。
    ///
    /// 認証なしの namespace では、Claude Code がサブスクの OAuth トークンを
    /// そのまま送ってくる。これが upstream へ抜けると、こちらが選んだ認証情報
    /// ではなく**クライアントのアカウント**で課金される。認証を掛けない以上、
    /// 剥がし漏れは事故に直結する (DR-0006)。
    #[tokio::test]
    async fn the_clients_own_credentials_never_reach_upstream() {
        const LEAKED: &str = "sk-ant-oat01-must-not-leak";

        // 転送先が自分で認証を持つ経路と、こちらが認証情報を付ける経路の両方を見る。
        for (credential, authenticated) in [
            ("", false),
            ("[credentials.a]\ntype = \"claude_oauth\"\n", true),
        ] {
            let (upstream, seen) = recording_upstream().await;
            let credential_ref = if credential.is_empty() {
                ""
            } else {
                "credential = \"a\""
            };
            // 認証を書かない namespace = 誰でも通る面。
            let base = serve(&format!(
                r#"
{credential}
[routes.a]
provider = "anthropic"
{credential_ref}
url = "{upstream}"
models = ["m"]

[[ns.open.routing]]
models = ["m"]
routes = ["a"]

[ns.open]
"#
            ))
            .await;

            let resp = reqwest::Client::new()
                .post(format!("{base}/ns-open/v1/messages"))
                .header("authorization", format!("Bearer {LEAKED}"))
                .header("x-api-key", LEAKED)
                .header("anthropic-version", "2023-06-01")
                .json(&request_body())
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                200,
                "認証なしの面なので通る (authenticated={authenticated})"
            );

            // 見るのは転送された分だけ (一覧の問い合わせと混ぜない)。
            let sent = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|req| req.starts_with("POST"))
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            assert!(
                !sent.is_empty(),
                "転送が届いている (authenticated={authenticated})"
            );
            assert!(
                !sent.contains(LEAKED),
                "名乗られた認証情報が漏れている (authenticated={authenticated}):\n{sent}"
            );
            assert!(
                !sent.contains("x-api-key"),
                "欄ごと落とす (authenticated={authenticated}):\n{sent}"
            );
            assert!(
                sent.contains("anthropic-version: 2023-06-01"),
                "関係のないヘッダまで落としてはいない (authenticated={authenticated}):\n{sent}"
            );

            if authenticated {
                assert!(
                    sent.contains("authorization: bearer tok"),
                    "こちらが選んだ認証情報に差し替わる:\n{sent}"
                );
            } else {
                assert!(
                    !sent.contains("authorization:"),
                    "転送先が自分で認証を持つ経路には何も付けない:\n{sent}"
                );
            }
        }
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
[routes.a]
provider = "anthropic"
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

#[cfg(test)]
mod stats_tests {
    use super::tests::{authed, fake_upstream, serve_with_default_ns};
    use super::*;
    use serde_json::json;

    /// 集計の置き場を試験用に差し替える設定断片。
    ///
    /// 指定しないと実運用の state ディレクトリを読み書きしてしまう。
    fn stats_dir(dir: &std::path::Path) -> String {
        format!("[stats]\ndir = \"{}\"\n", dir.display())
    }

    /// 転送 1 本で集計が現れる。
    #[tokio::test]
    async fn a_forwarded_request_shows_up_in_the_stats() {
        let upstream = fake_upstream(|| {
            (
                200,
                r#"{"type":"message","content":[],"usage":{"input_tokens":18,
                    "output_tokens":16,"cache_creation_input_tokens":2,
                    "cache_read_input_tokens":3}}"#
                    .to_owned(),
                vec![("content-type".into(), "application/json".into())],
            )
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let base = serve_with_default_ns(&format!(
            r#"
{}
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#,
            stats_dir(dir.path())
        ))
        .await;

        let resp = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&json!({
                    "model": "m",
                    "max_tokens": 8,
                    "messages": [{"role": "user", "content": "hi"}],
                })),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        // 本文を読み切って、集計が確定するまで待つ。
        let _ = resp.text().await.unwrap();

        let report: Value = reqwest::get(format!("{base}/llm-gateway/stats"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let days = report["days"].as_object().expect("日ごとの表");
        assert_eq!(days.len(), 1, "1 日分: {report}");
        let counters = &days.values().next().unwrap()["credentials"]["-"]["m"];
        assert_eq!(counters["requests"], 1);
        // トークンは区分ごとの表で出る (DR-0014 §4 の正規レコード)。
        assert_eq!(
            counters["tokens"],
            json!({
                "input": 18,
                "output": 16,
                "input.cache_creation": 2,
                "input.cache_read": 3,
            }),
            "{report}"
        );
    }

    /// SSE でも集計され、流れるバイト列は変わらない。
    #[tokio::test]
    async fn a_streamed_response_is_counted_without_being_altered() {
        let sse = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30,\"output_tokens\":1}}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":30,\"output_tokens\":40}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

        let upstream = fake_upstream(move || {
            (
                200,
                sse.to_owned(),
                vec![("content-type".into(), "text/event-stream".into())],
            )
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let base = serve_with_default_ns(&format!(
            r#"
{}
[routes.a]
provider = "anthropic"
url = "{upstream}"
models = ["m"]

[[ns.default.routing]]
models = ["m"]
routes = ["a"]
"#,
            stats_dir(dir.path())
        ))
        .await;

        let body = authed(
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .json(&json!({"model": "m", "stream": true, "messages": []})),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

        assert_eq!(body, sse, "覗いても 1 バイトも変えない");

        let report: Value = reqwest::get(format!("{base}/llm-gateway/stats"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let counters =
            &report["days"].as_object().unwrap().values().next().unwrap()["credentials"]["-"]["m"];
        assert_eq!(counters["tokens"]["input"], 30);
        assert_eq!(
            counters["tokens"]["output"], 40,
            "累積の最終値。message_start の 1 を足さない"
        );
    }

    /// 何も通していなければ空で返る (壊れずに)。
    #[tokio::test]
    async fn stats_are_empty_before_anything_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_with_default_ns(&stats_dir(dir.path())).await;

        let resp = reqwest::get(format!("{base}/llm-gateway/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let report: Value = resp.json().await.unwrap();
        assert!(report["days"].as_object().unwrap().is_empty(), "{report}");
        assert!(
            report["generated_at_iso"].is_string(),
            "いつ作ったか: {report}"
        );
    }

    /// 読めない `days` は断る。黙って既定に落とすと、絞ったつもりの相手が
    /// 別の範囲を見ることになる。
    #[tokio::test]
    async fn an_unreadable_days_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_with_default_ns(&stats_dir(dir.path())).await;

        let resp = reqwest::get(format!("{base}/llm-gateway/stats?days=lots"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        assert!(resp.text().await.unwrap().contains("days"));
    }

    #[test]
    fn days_defaults_to_a_week() {
        assert_eq!(requested_days(None), Ok(DEFAULT_DAYS));
        assert_eq!(requested_days(Some("")), Ok(DEFAULT_DAYS));
        assert_eq!(requested_days(Some("refresh=true")), Ok(DEFAULT_DAYS));
        assert_eq!(requested_days(Some("days=30")), Ok(30));
        assert_eq!(requested_days(Some("days=0")), Ok(0));
        assert!(requested_days(Some("days=-1")).is_err());
        assert!(requested_days(Some("days=")).is_err());
    }

    /// 大きすぎる日数は上限で抑える。
    ///
    /// 抑えないと日数を秒に直す掛け算が桁あふれし、絞り込みの起点が未来に
    /// 回って**何も返らない**。
    #[test]
    fn an_enormous_day_count_is_clamped() {
        let max = llm_gateway::stats::MAX_DAYS;
        assert_eq!(requested_days(Some("days=100000")), Ok(max));
        assert_eq!(
            requested_days(Some(&format!("days={}", usize::MAX))),
            Ok(max)
        );
        assert_eq!(requested_days(Some("days=36500")), Ok(max), "上限そのもの");
    }

    /// 極端な日数でも 200 が返る (落ちない)。
    #[tokio::test]
    async fn an_enormous_day_count_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_with_default_ns(&stats_dir(dir.path())).await;

        let resp = reqwest::get(format!("{base}/llm-gateway/stats?days={}", usize::MAX))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}
