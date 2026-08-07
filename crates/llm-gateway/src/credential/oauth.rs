//! OAuth token の取得と更新。
//!
//! Anthropic と ChatGPT で endpoint と client_id が違うだけで、更新の手順は同じ:
//! `refresh_token` を送ると新しい access token が返り、**refresh token 自体も
//! 入れ替わる**。返ってきた値を保存し損ねると、次の更新で弾かれて
//! 再ログインが要る状態になる。
//!
//! 取得 ([`begin`] 以降) は provider ごとに要求するものが違うので、差分を
//! [`AuthProfile`] に集めてある。

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize as _, Zeroizing};

use crate::{Error, Result};

use super::stored::{CodexTokens, OauthTokens, Payload};
use super::time::{format_rfc3339, now_unix};
use super::{CredentialId, Kind, StoredCredential};

/// Anthropic (Claude サブスク) の更新先。
pub const ANTHROPIC_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
pub const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// ChatGPT (Codex サブスク) の更新先。
pub const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// 種別ごとの更新先。
pub fn endpoint_for(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Claude => (ANTHROPIC_TOKEN_URL, ANTHROPIC_CLIENT_ID),
        Kind::Codex => (OPENAI_TOKEN_URL, OPENAI_CLIENT_ID),
    }
}

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

/// 更新の応答。
///
/// `refresh_token` が省かれる実装もありうるので、その場合は元の値を使い回す
/// (入れ替わっていないとみなす)。
#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    /// 有効期間 (秒)。省かれた場合は保存済みの期限を維持する。
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub account: Option<Account>,
}

#[derive(Debug, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub email_address: Option<String>,
}

/// token を更新する。
///
/// この関数は保存を行わない。呼び出し側 (single-flight の内側) が
/// 保存まで面倒を見る。
pub async fn refresh(
    http: &reqwest::Client,
    id: &CredentialId,
    kind: Kind,
    refresh_token: &str,
) -> Result<RefreshResponse> {
    refresh_at(http, id, kind, refresh_token, None).await
}

/// 更新先を指定して token を更新する。
///
/// `url_override` は試験用。本番は [`refresh`] を使い、種別から決まる
/// 既定の更新先へ送る。
pub async fn refresh_at(
    http: &reqwest::Client,
    id: &CredentialId,
    kind: Kind,
    refresh_token: &str,
    url_override: Option<&str>,
) -> Result<RefreshResponse> {
    let (default_url, client_id) = endpoint_for(kind);
    let url = url_override.unwrap_or(default_url);
    let body = RefreshRequest {
        client_id,
        grant_type: "refresh_token",
        refresh_token,
    };

    let resp = http
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Refresh {
            id: id.to_string(),
            reason: format!("{url} に接続できません: {e}"),
        })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| Error::Refresh {
        id: id.to_string(),
        reason: format!("応答を読めません: {e}"),
    })?;

    if !status.is_success() {
        return Err(Error::Refresh {
            id: id.to_string(),
            reason: refresh_failure_reason(status.as_u16(), &text),
        });
    }

    serde_json::from_str(&text).map_err(|e| Error::Refresh {
        id: id.to_string(),
        reason: format!("応答の形式が想定と違います: {e}"),
    })
}

/// 失敗の理由を、次に何をすればよいか分かる形にする。
///
/// 応答本文をそのまま出すと token が混ざる恐れがあるので、状態から言い換える。
fn refresh_failure_reason(status: u16, body: &str) -> String {
    if body.contains("refresh_token_reused") {
        return "refresh token が既に使用済みです。更新が二重に走った可能性があります。\
再ログインが必要です"
            .to_owned();
    }
    if body.contains("refresh_token_expired") || body.contains("refresh_token_invalidated") {
        return "refresh token が失効しています。再ログインが必要です".to_owned();
    }
    match status {
        400 | 401 => "refresh token が受け付けられませんでした。再ログインが必要です".to_owned(),
        429 => "更新の要求が多すぎます。しばらく待ってから再試行してください".to_owned(),
        500..=599 => format!("更新先が {status} を返しました。一時的な障害の可能性があります"),
        _ => format!("更新先が {status} を返しました"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// ここから下は認可 (login)。更新と client_id / 交換先 / 応答の形を共有する。
// ─────────────────────────────────────────────────────────────────────

/// 認可の入口。人がブラウザで開く画面。
pub const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";

/// 戻りを待つ上限。
///
/// 人が画面を操作する時間に加えて、リモートから ssh port forward を張り直す
/// ような段取りも入りうるので長めに取る (5 分では足りなかった。実測 2026-07-28)。
const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// 結果を渡してから応答を返し切るのを待つ上限。
///
/// 交換・確認・保存はこれを待つ前に終わっているので、ここに要るのは
/// 出来上がった画面を書き出す分だけ。ここを待たずに閉じるとブラウザに白紙が出る。
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 交換・確認の 1 往復に許す上限。
///
/// ブラウザはこの間ずっと応答を待つので、上限が無いと画面が固まったままになる。
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// token が使えるか確かめる先。
///
/// **認可の発行元に固定する**。config の url を書き換えていてもそちらは見ない —
/// ここで確かめたいのは受け取った token 自体であって、転送先の設定ではない。
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const OPENAI_USERINFO_URL: &str = "https://auth.openai.com/api/accounts/oauth/userinfo";

/// 種別ごとの認可の作法。
///
/// 交換先 ([`endpoint_for`]) と違い、認可の入口は provider ごとに要求するものが
/// 違う (scope の語彙・戻り先・独自パラメータ)。差分をここ 1 箇所に集める。
#[derive(Debug, Clone, Copy)]
pub struct AuthProfile {
    pub authorize_url: &'static str,
    /// 戻り先のポートとパス。
    ///
    /// **client_id ごとに登録済みの値しか使えない**ので、空きポートを拾う形には
    /// できない。塞がっていたら諦めて、塞いでいる側を止めてもらう。
    pub redirect_port: u16,
    pub redirect_path: &'static str,
    pub scope: &'static str,
    /// provider 固有の追加パラメータ。
    pub extra_params: &'static [(&'static str, &'static str)],
    /// 認可コードを交換するときの body の形式。
    ///
    /// 更新 (`refresh`) は両者とも JSON で通るが、**交換は揃っていない**。
    /// ChatGPT は form でないと 400 を返す (実測 2026-07-28)。
    pub exchange_form: ExchangeForm,
    /// 交換の body に `state` を載せるか。
    ///
    /// `state` は認可エンドポイント専用で token エンドポイントには定義が無い。
    /// Anthropic は受け取るが、ChatGPT は載せると 400 を返す。
    pub exchange_state: bool,
}

/// 認可コードの交換で送る body の形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeForm {
    Json,
    UrlEncoded,
}

impl AuthProfile {
    /// 認可と交換の両方に送る戻り先。食い違うと交換で弾かれる。
    pub fn redirect_uri(&self) -> String {
        format!(
            "http://localhost:{}{}",
            self.redirect_port, self.redirect_path
        )
    }
}

/// 種別ごとの認可の作法を返す。
pub fn auth_profile_for(kind: Kind) -> AuthProfile {
    match kind {
        Kind::Claude => AuthProfile {
            authorize_url: ANTHROPIC_AUTHORIZE_URL,
            redirect_port: 54545,
            redirect_path: "/callback",
            // `user:file_upload` は単数形。複数形にすると認可画面が
            // 「不明なスコープ」で弾く (実測 2026-07-28)。
            scope: "user:profile user:inference user:sessions:claude_code \
                    user:mcp_servers user:file_upload",
            extra_params: &[],
            exchange_form: ExchangeForm::Json,
            exchange_state: true,
        },
        Kind::Codex => AuthProfile {
            authorize_url: OPENAI_AUTHORIZE_URL,
            // Design rationale: Claude 側 (54545 / `/callback`) と揃えていない。
            // 使っている client_id は Codex CLI のものなので、戻り先も Codex CLI が
            // 登録している値でないと認可の時点で弾かれる。揃える利点より、
            // 登録済みの値であることのほうが優先される。
            redirect_port: 1455,
            redirect_path: "/auth/callback",
            // offline_access が refresh token を出させる分。これが無いと
            // 8 時間ごとに人の操作が要る。
            scope: "openid profile email offline_access api.connectors.read api.connectors.invoke",
            // 独自パラメータ。id_token に組織情報を載せさせ、Codex CLI 用の
            // 簡易フローに乗る。originator は ChatGPT 経路が要求する値と同じ
            // (DR-0002 の接続仕様)。
            extra_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "codex_cli_rs"),
            ],
            exchange_form: ExchangeForm::UrlEncoded,
            exchange_state: false,
        },
    }
}

/// 認可の待ち受け。
///
/// [`begin`] が返った時点で受け口は開いている。URL を出してから人が開くまでの
/// 間に戻ってこられても取りこぼさない。
pub struct Authorization {
    kind: Kind,
    profile: AuthProfile,
    url: String,
    state: String,
    verifier: Zeroizing<String>,
    listener: tokio::net::TcpListener,
}

impl Authorization {
    /// ブラウザで開く URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 戻りを待ち、token の交換・確認・保存まで済ませてから結果を返す。
    ///
    /// `save` は確認を通った token を保存する処理。**ブラウザに出す画面は
    /// ここまで全部済んでから決まる**。認可コードを受け取った時点で
    /// 「認可できました」を出すと、その後の交換や保存が失敗しても画面には
    /// 成功が残る。
    pub async fn finish<T>(self, save: impl FnOnce(&Tokens) -> Result<T>) -> Result<T> {
        self.finish_with(Endpoints::default(), save).await
    }

    /// 接続先を指定して仕上げる。
    ///
    /// `endpoints` は試験用。本番は [`Authorization::finish`] を使い、
    /// 種別から決まる交換先・確認先へ送る。
    async fn finish_with<T>(
        self,
        endpoints: Endpoints<'_>,
        save: impl FnOnce(&Tokens) -> Result<T>,
    ) -> Result<T> {
        let Authorization {
            kind,
            profile,
            state,
            verifier,
            listener,
            ..
        } = self;
        let mut callback = Callback::start(listener, profile.redirect_path, &state);

        let handoff = match callback.wait().await {
            Ok(handoff) => handoff,
            // 受け取れなかった戻りの画面は handler が出している。畳むだけ。
            Err(e) => {
                callback.shutdown().await;
                return Err(e);
            }
        };

        let result = async {
            let tokens = exchange_code(
                kind,
                &profile,
                &handoff.code,
                &state,
                &verifier,
                endpoints.exchange,
            )
            .await?;
            verify(kind, &tokens, endpoints.verify).await?;
            save(&tokens)
        }
        .await;

        // ここで初めてブラウザに結果が出る。
        let reply = result.as_ref().map(|_| ()).map_err(ToString::to_string);
        let _ = handoff.reply.send(reply);
        callback.shutdown().await;
        result
    }
}

/// 交換と確認の接続先。試験で手元のサーバへ寄せるために持つ。
#[derive(Debug, Clone, Copy, Default)]
struct Endpoints<'a> {
    exchange: Option<&'a str>,
    verify: Option<&'a str>,
}

/// 認可を始める。戻り先を開き、ブラウザで開く URL を組み立てる。
pub async fn begin(kind: Kind) -> Result<Authorization> {
    let profile = auth_profile_for(kind);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", profile.redirect_port))
        .await
        .map_err(|e| Error::Login {
            reason: format!(
                "{} で待ち受けられません: {e}。\
この戻り先は client_id に登録済みの値なので他のポートでは代用できません。\
このポートを使っている別のログイン処理 (cpa など) を止めてから、もう一度実行してください",
                profile.redirect_uri()
            ),
        })?;
    Ok(begin_on(kind, profile, listener))
}

/// 受け口を指定して始める。試験で任意のポートに寄せるために分けてある。
fn begin_on(kind: Kind, profile: AuthProfile, listener: tokio::net::TcpListener) -> Authorization {
    let verifier = Zeroizing::new(random_token());
    let state = random_token();
    let url = authorize_url(&profile, kind, &state, &challenge_of(&verifier));
    Authorization {
        kind,
        profile,
        url,
        state,
        verifier,
        listener,
    }
}

/// 認可 URL を組み立てる。
fn authorize_url(profile: &AuthProfile, kind: Kind, state: &str, challenge: &str) -> String {
    let (_, client_id) = endpoint_for(kind);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &profile.redirect_uri())
        .append_pair("scope", profile.scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .extend_pairs(profile.extra_params.iter().copied())
        .finish();
    format!("{}?{query}", profile.authorize_url)
}

/// 推測されない値を作る。
///
/// PKCE の verifier と state は、どちらも「横から見ている側が再現できないこと」
/// が要件。再現されると認可を横取りされるので、暗号論的に安全な生成器を使う
/// (`rand::fill` の裏は OS 由来の種で回る ChaCha)。
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    let token = B64URL.encode(bytes);
    bytes.zeroize();
    token
}

/// PKCE の code_challenge (S256)。
fn challenge_of(verifier: &str) -> String {
    use sha2::{Digest as _, Sha256};
    B64URL.encode(Sha256::digest(verifier.as_bytes()))
}

/// ブラウザからの戻りを 1 回だけ受ける待ち受け。
///
/// 受けた時点では応答を返さない。交換・確認・保存の結果を [`Handoff::reply`]
/// で受け取ってから画面を作るので、それまで待ち受けを開けたままにしておく。
struct Callback {
    server: tokio::task::JoinHandle<()>,
    done: Arc<tokio::sync::Notify>,
    handoff: tokio::sync::oneshot::Receiver<CallbackOutcome>,
}

impl Callback {
    fn start(listener: tokio::net::TcpListener, path: &str, expected_state: &str) -> Self {
        let (tx, handoff) = tokio::sync::oneshot::channel();
        let done = Arc::new(tokio::sync::Notify::new());
        let state = Arc::new(CallbackState {
            expected: expected_state.to_owned(),
            sender: tokio::sync::Mutex::new(Some(tx)),
        });

        let app = axum::Router::new()
            .route(path, axum::routing::get(receive))
            .with_state(state);
        // graceful なので、閉じる合図を出しても返事を返し切ってから終わる。
        let shutdown = Arc::clone(&done);
        let serving = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.notified().await });
        let server = tokio::spawn(async move {
            let _ = serving.await;
        });

        Self {
            server,
            done,
            handoff,
        }
    }

    /// 戻りを待つ。
    ///
    /// 受け口ごと倒れたら送り口も落ちるので、待ちっぱなしにはならない。
    /// 受け口の生死を別に見張る必要はない。
    async fn wait(&mut self) -> Result<Handoff> {
        let outcome = match tokio::time::timeout(CALLBACK_TIMEOUT, &mut self.handoff).await {
            Ok(received) => received.unwrap_or_else(|_| Err("認可の受け口が落ちました".to_owned())),
            Err(_) => Err(format!(
                "{} 秒待っても戻ってきませんでした。\
ブラウザで許可まで進めてから、もう一度実行してください",
                CALLBACK_TIMEOUT.as_secs()
            )),
        };
        outcome.map_err(|reason| Error::Login { reason })
    }

    /// 待ち受けを畳む。画面を出し切ってから終わる。
    async fn shutdown(mut self) {
        self.done.notify_one();
        if tokio::time::timeout(FLUSH_TIMEOUT, &mut self.server)
            .await
            .is_err()
        {
            self.server.abort();
        }
    }
}

/// 戻りを 1 つだけ受け取るための置き場。
struct CallbackState {
    expected: String,
    /// 1 回で使い切る。2 回目以降の要求には結果を渡さない。
    sender: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<CallbackOutcome>>>,
}

/// handler からメインの流れへ渡すもの。
struct Handoff {
    code: String,
    /// 交換・確認・保存の結果を返す口。ここに届くまで handler は画面を出さない。
    reply: tokio::sync::oneshot::Sender<FinishOutcome>,
}

type CallbackOutcome = std::result::Result<Handoff, String>;

/// 画面に出すところまで来た結果。失敗の理由はそのまま文言になる。
type FinishOutcome<T = ()> = std::result::Result<T, String>;

/// ブラウザからの戻りを受ける。
async fn receive(
    axum::extract::State(state): axum::extract::State<Arc<CallbackState>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> axum::response::Html<String> {
    let code = match code_from_query(query.as_deref(), &state.expected) {
        Ok(code) => code,
        // 交換まで進めないので、ここで結果が出揃っている。
        Err(reason) => {
            if let Some(tx) = state.sender.lock().await.take() {
                let _ = tx.send(Err(reason.clone()));
            }
            return axum::response::Html(result_page(&Err(reason)));
        }
    };

    let Some(tx) = state.sender.lock().await.take() else {
        return axum::response::Html(result_page(&Err(
            "この認可は受け付け済みです。結果は端末の表示を確認してください".to_owned(),
        )));
    };

    let (reply, result) = tokio::sync::oneshot::channel();
    if tx.send(Ok(Handoff { code, reply })).is_err() {
        return axum::response::Html(result_page(&Err(
            "認可の待ち受けが既に閉じられています。もう一度 login をやり直してください".to_owned(),
        )));
    }

    // 交換・確認・保存が終わるまで待つ。ここで待たずに画面を出すと、
    // その後で失敗しても「認可できました」が残ってしまう。
    let outcome = result
        .await
        .unwrap_or_else(|_| Err("認可の処理が最後まで進みませんでした".to_owned()));
    axum::response::Html(result_page(&outcome))
}

/// 戻りの query から認可コードを取り出す。
///
/// state が一致しない戻りは捨てる。一致を見ないと、別の誰かに始めさせた認可の
/// 結果を掴まされる (CSRF)。掴んだ token は相手のアカウントのものになる。
fn code_from_query(query: Option<&str>, expected_state: &str) -> FinishOutcome<String> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;

    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        let detail = description.unwrap_or(error);
        return Err(format!("認可が拒否されました: {detail}"));
    }
    if state.as_deref() != Some(expected_state) {
        return Err(
            "戻りの state が送った値と違います。この認可の戻りではないので受け取りません"
                .to_owned(),
        );
    }
    match code {
        Some(code) if !code.is_empty() => Ok(code),
        _ => Err("戻りに認可コードが付いていません".to_owned()),
    }
}

/// ブラウザに出す結果の画面。
///
/// 認可コードも token も出さない。画面はブラウザの履歴や共有画面に残る。
fn result_page(outcome: &FinishOutcome) -> String {
    let body = match outcome {
        Ok(()) => "<h1>認可できました</h1>\
<p>token を取得し、使えることを確認して保存しました。この画面は閉じて、端末に戻ってください。</p>"
            .to_owned(),
        Err(reason) => format!(
            "<h1>認可できませんでした</h1><p>{}</p>",
            html_escape(reason)
        ),
    };
    format!(
        "<!doctype html><html lang=\"ja\"><head><meta charset=\"utf-8\">\
<title>llm-gateway</title></head><body>{body}</body></html>"
    )
}

/// 文言をそのまま画面に置けるようにする。
///
/// 理由の文言には upstream 由来の値 (`error_description`) が混ざる。
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 認可で受け取った token 一式。
#[derive(Debug)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// ChatGPT が返す identity token。refresh で部分更新される。
    pub id_token: Option<String>,
    /// 有効期間 (秒)。
    pub expires_in: i64,
    pub email: Option<String>,
    /// ChatGPT 経路が要求するアカウント識別子。
    pub account_id: Option<String>,
}

impl Drop for Tokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        if let Some(id_token) = &mut self.id_token {
            id_token.zeroize();
        }
    }
}

impl Tokens {
    /// 保存する形にする。
    pub fn to_stored(&self, kind: Kind, base: Option<&StoredCredential>) -> StoredCredential {
        self.to_stored_at(kind, base, now_unix())
    }

    /// 時刻を指定して保存する形にする。`now_unix` は試験用。
    ///
    /// `base` は再ログイン時の既存の内容。priority / disabled / 除外リスト /
    /// 拒否された beta の記録は認可で決まるものではなく運用側の値なので、
    /// 上書きせず引き継ぐ (消すと、再ログインのたびにモデルの割り当てが崩れる)。
    ///
    /// payload は引き継がない。認可を通し直して受け取ったものが全てで、
    /// 前回の残骸を混ぜる理由が無い。ただし email / account_id は今回の応答で
    /// 分からなかったときだけ前の値を残す — どちらもアカウントの identity で、
    /// 応答に無かったことは「変わった」を意味しない。
    pub fn to_stored_at(
        &self,
        kind: Kind,
        base: Option<&StoredCredential>,
        now_unix: i64,
    ) -> StoredCredential {
        let previous = base.map(|b| &b.payload);
        let email = self
            .email
            .clone()
            .or_else(|| previous.map(|p| p.email().to_owned()))
            .unwrap_or_default();

        let tokens = OauthTokens {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expired: format_rfc3339(now_unix + self.expires_in),
            email,
            extra: BTreeMap::new(),
        };

        let payload = match kind {
            Kind::Claude => Payload::ClaudeOauth(tokens),
            Kind::Codex => Payload::CodexOauth(CodexTokens {
                oauth: tokens,
                id_token: self.id_token.clone().or_else(|| {
                    previous.and_then(|payload| match payload {
                        Payload::CodexOauth(tokens) => tokens.id_token.clone(),
                        _ => None,
                    })
                }),
                account_id: self
                    .account_id
                    .clone()
                    .or_else(|| previous.and_then(|p| p.account_id().map(str::to_owned))),
            }),
        };

        let mut next = match base {
            Some(base) => {
                let mut next = base.clone();
                next.payload = payload;
                next
            }
            None => StoredCredential::new(payload),
        };
        next.last_refresh = format_rfc3339(now_unix);
        next
    }
}

/// 交換の要求。
///
/// `state` は認可エンドポイント専用のパラメータで、token エンドポイントには
/// RFC 6749 の定義が無い。Anthropic は受け取るが、ChatGPT は送ると 400 を返す
/// (実測 2026-07-28)。要求する側にだけ送る。
#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
}

/// 交換の応答。
///
/// 更新の応答 ([`RefreshResponse`]) と形は近いが、`id_token` はここでしか
/// 使わない (ChatGPT のアカウント識別子がこの中にしか入っていない)。
#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    account: Option<Account>,
    #[serde(default)]
    id_token: Option<String>,
}

/// login で使う HTTP クライアント。
///
/// 更新と違い、共有のものを持ち回らない (login は 1 回で終わる)。ブラウザが
/// 応答を待っている間の往復なので、上限を切っておかないと画面が固まる。
fn login_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(UPSTREAM_TIMEOUT)
        .build()
        .map_err(|e| Error::Login {
            reason: format!("HTTP クライアントを作れません: {e}"),
        })
}

/// 受け取った token が実際に使えるか確かめる。
///
/// 交換が通っただけでは「使える token を持っている」とは言えない。保存して
/// から初めての転送で弾かれるより、ここで分かったほうが直しようがある。
///
/// - Claude: モデル一覧。起動時に router が聞きに行くのと同じ口なので、
///   ここが通らない token では、その認証情報からモデルを 1 つも公開できない。
/// - Codex: 認可の発行元が公開している OIDC の userinfo。ChatGPT の
///   Responses API には、推論を走らせずに叩ける口が無い。`openid` と `email`
///   を認可済みなので、この token でそのまま通る。
async fn verify(kind: Kind, tokens: &Tokens, url_override: Option<&str>) -> Result<()> {
    let default_url = match kind {
        Kind::Claude => ANTHROPIC_MODELS_URL,
        Kind::Codex => OPENAI_USERINFO_URL,
    };
    let url = url_override.unwrap_or(default_url);

    let mut request = login_client()?
        .get(url)
        .header("authorization", format!("Bearer {}", tokens.access_token));
    if kind == Kind::Claude {
        request = request.header("anthropic-version", "2023-06-01");
    }

    let resp = request.send().await.map_err(|e| Error::Login {
        reason: format!("取得した token を確かめられません ({url} に接続できません: {e})"),
    })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| Error::Login {
        reason: format!("取得した token を確かめられません (応答を読めません: {e})"),
    })?;

    if !status.is_success() {
        return Err(Error::Login {
            reason: verify_failure_reason(status.as_u16()),
        });
    }

    if kind == Kind::Claude {
        let models =
            crate::discovery::parse(crate::discovery::Flavor::Anthropic, &text).map_err(|e| {
                Error::Login {
                    reason: format!("モデル一覧の形式が想定と違います: {e}"),
                }
            })?;
        if models.is_empty() {
            return Err(Error::Login {
                reason: "token は使えますが、このアカウントで使えるモデルが 1 つもありません。\
サブスクの契約状況を確認してください"
                    .to_owned(),
            });
        }
    }

    Ok(())
}

/// 確認に失敗した理由。
///
/// 応答本文は出さない。ここでの本文は認証済みの応答なので、アカウントの
/// 情報が混ざりうる。
fn verify_failure_reason(status: u16) -> String {
    match status {
        401 | 403 => "取得した token が upstream に受け付けられませんでした。\
認可したアカウントにこのサブスクの利用権があるか確認してから、login をやり直してください"
            .to_owned(),
        429 => "確認先が要求の多さを理由に断りました。\
しばらく待ってから login をやり直してください"
            .to_owned(),
        500..=599 => format!(
            "確認先が {status} を返しました。一時的な障害の可能性があります。\
しばらく待ってから login をやり直してください"
        ),
        other => format!("取得した token を確かめられません (確認先が {other} を返しました)"),
    }
}

/// 認可コードを token に交換する。
async fn exchange_code(
    kind: Kind,
    profile: &AuthProfile,
    code: &str,
    state: &str,
    verifier: &str,
    url_override: Option<&str>,
) -> Result<Tokens> {
    let (default_url, client_id) = endpoint_for(kind);
    let url = url_override.unwrap_or(default_url);
    let body = ExchangeRequest {
        client_id,
        grant_type: "authorization_code",
        code,
        redirect_uri: &profile.redirect_uri(),
        code_verifier: verifier,
        state: profile.exchange_state.then_some(state),
    };

    let http = login_client()?;

    let request = http.post(url);
    let request = match profile.exchange_form {
        ExchangeForm::Json => request.json(&body),
        ExchangeForm::UrlEncoded => request.form(&body),
    };
    let resp = request.send().await.map_err(|e| Error::Login {
        reason: format!("{url} に接続できません: {e}"),
    })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| Error::Login {
        reason: format!("応答を読めません: {e}"),
    })?;

    if !status.is_success() {
        return Err(Error::Login {
            reason: exchange_failure_reason(status.as_u16(), &text),
        });
    }

    let parsed: ExchangeResponse = serde_json::from_str(&text).map_err(|e| Error::Login {
        reason: format!("応答の形式が想定と違います: {e}"),
    })?;
    tokens_from(kind, parsed)
}

/// 応答を保存できる形にする。
fn tokens_from(kind: Kind, resp: ExchangeResponse) -> Result<Tokens> {
    let Some(refresh_token) = resp.refresh_token else {
        return Err(Error::Login {
            reason: "refresh token が返りませんでした。\
これが無いと 8 時間ごとに再ログインが要るので、認可し直してください"
                .to_owned(),
        });
    };

    let claims = resp.id_token.as_deref().and_then(jwt_claims);
    let email = resp
        .account
        .and_then(|a| a.email_address)
        .or_else(|| claims.as_ref().and_then(email_of));
    let account_id = claims.as_ref().and_then(account_id_of);
    if kind == Kind::Codex && account_id.is_none() {
        return Err(Error::Login {
            reason: "ChatGPT の応答にアカウント識別子がありません。認可をやり直してください"
                .to_owned(),
        });
    }

    Ok(Tokens {
        access_token: resp.access_token,
        refresh_token,
        id_token: resp.id_token,
        // 返らなかった場合は期限切れ扱いにする。初回の使用で更新が走るだけで
        // 済み、認可をやり直させずに回復できる。
        expires_in: resp.expires_in.unwrap_or(0),
        email,
        account_id,
    })
}

/// JWT の payload を読む。
///
/// 署名は検証しない。今この交換で受け取ったばかりの応答なので、間に立って
/// 差し替えられる相手がいない。検証するには公開鍵の取得と失効管理が要るが、
/// この用途では得るものがない。
fn jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let raw = B64URL.decode(payload).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// ChatGPT のアカウント識別子。provider 独自の claim に入っている。
fn account_id_of(claims: &Value) -> Option<String> {
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub(crate) fn account_id_from_token(token: &str) -> Option<String> {
    jwt_claims(token).as_ref().and_then(account_id_of)
}

fn email_of(claims: &Value) -> Option<String> {
    claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// 交換に失敗した理由を、次に何をすればよいか分かる形にする。
///
/// 応答本文は受け取らない。認可コードや token が混ざったものを、そのまま
/// 文言に出さないための形。
fn exchange_failure_reason(status: u16, body: &str) -> String {
    let base = match status {
        400 => "認可コードが受け付けられませんでした。\
使い切ったか期限切れの可能性があります。もう一度 login をやり直してください"
            .to_owned(),
        401 | 403 => "認可が拒否されました。\
ログインしたアカウントにこのサブスクの利用権があるか確認してください"
            .to_owned(),
        429 => "要求が多すぎます。しばらく待ってから再試行してください".to_owned(),
        500..=599 => format!("交換先が {status} を返しました。一時的な障害の可能性があります"),
        other => format!("交換先が {other} を返しました"),
    };
    match oauth_error_detail(body) {
        Some(detail) => format!("{base} (交換先の応答: {detail})"),
        None => base,
    }
}

/// 失敗応答から原因を取り出す。
///
/// OAuth の失敗応答 (RFC 6749 §5.2) は `error` と `error_description` を持ち、
/// token は含まれない。状態から言い換えるだけだと「何が違うのか」が分からず
/// 推測で直す羽目になるので、upstream が言っていることをそのまま添える。
fn oauth_error_detail(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct OauthError {
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
    }
    let parsed: OauthError = serde_json::from_str(body).ok()?;
    match (parsed.error, parsed.error_description) {
        (Some(e), Some(d)) => Some(format!("{e}: {d}")),
        (Some(e), None) => Some(e),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_differ_by_kind() {
        let (url, id) = endpoint_for(Kind::Claude);
        assert_eq!(url, "https://api.anthropic.com/v1/oauth/token");
        assert_eq!(id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");

        let (url, id) = endpoint_for(Kind::Codex);
        assert_eq!(url, "https://auth.openai.com/oauth/token");
        assert_eq!(id, "app_EMoamEEZ73f0CkXaXp7hrann");
    }

    /// 送るのは JSON。form ではない (upstream の実装に合わせてある)。
    #[test]
    fn request_is_json_with_three_fields() {
        let body = RefreshRequest {
            client_id: "cid",
            grant_type: "refresh_token",
            refresh_token: "rt",
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();
        assert_eq!(v["client_id"], "cid");
        assert_eq!(v["grant_type"], "refresh_token");
        assert_eq!(v["refresh_token"], "rt");
    }

    #[test]
    fn parses_anthropic_response() {
        let raw = r#"{
            "access_token": "new-at",
            "refresh_token": "new-rt",
            "expires_in": 28800,
            "account": {"email_address": "someone@example.com"}
        }"#;
        let r: RefreshResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.access_token.as_deref(), Some("new-at"));
        assert_eq!(r.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(r.expires_in, Some(28_800), "Anthropic は 8 時間");
        assert_eq!(
            r.account.and_then(|a| a.email_address).as_deref(),
            Some("someone@example.com")
        );
    }

    /// refresh は返された項目だけを置換するため、全項目を省ける形で読む。
    #[test]
    fn refresh_response_is_partial() {
        let response: RefreshResponse = serde_json::from_str("{}").unwrap();
        assert!(response.access_token.is_none());
        assert!(response.refresh_token.is_none());
        assert!(response.id_token.is_none());
        assert!(response.expires_in.is_none());
        assert!(response.account.is_none());
    }

    /// 二重更新を踏んだと分かる文言になっているか。
    #[test]
    fn reused_token_is_explained() {
        let reason = refresh_failure_reason(
            400,
            r#"{"error":"invalid_grant","error_description":"refresh_token_reused"}"#,
        );
        assert!(reason.contains("二重"), "{reason}");
        assert!(reason.contains("再ログイン"), "{reason}");
    }

    /// 一時障害と恒久失敗を言い分ける (再試行してよいかが変わる)。
    #[test]
    fn distinguishes_transient_from_permanent() {
        assert!(refresh_failure_reason(503, "").contains("一時的"));
        assert!(refresh_failure_reason(401, "").contains("再ログイン"));
        assert!(refresh_failure_reason(429, "").contains("待って"));
    }

    /// 応答本文をそのまま埋め込まない (token が混ざりうるため)。
    #[test]
    fn does_not_echo_response_body() {
        let body = r#"{"error":"invalid_grant","access_token":"leaked-secret-value"}"#;
        let reason = refresh_failure_reason(400, body);
        assert!(!reason.contains("leaked-secret-value"), "{reason}");
    }

    // ── 認可 (login) ──

    /// RFC 7636 Appendix B の値。ここが合わなければ PKCE は成立しない。
    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// verifier / state は毎回違い、URL にそのまま置ける文字だけで出来ている。
    #[test]
    fn random_token_is_unpredictable_and_url_safe() {
        let a = random_token();
        let b = random_token();

        assert_ne!(a, b, "毎回同じなら state もPKCEも意味がない");
        assert_eq!(a.len(), 43, "32 バイトを base64url にした長さ");
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "{a}"
        );
    }

    /// 戻り先は種別ごとに違う (client_id ごとに登録済みの値しか使えない)。
    #[test]
    fn redirect_uri_differs_by_kind() {
        assert_eq!(
            auth_profile_for(Kind::Claude).redirect_uri(),
            "http://localhost:54545/callback"
        );
        assert_eq!(
            auth_profile_for(Kind::Codex).redirect_uri(),
            "http://localhost:1455/auth/callback"
        );
    }

    fn query_of(url: &str) -> std::collections::HashMap<String, String> {
        let query = url.split_once('?').expect("query がある").1;
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// 認可 URL に PKCE と state が載る。S256 でないと平文の verifier が漏れる。
    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let profile = auth_profile_for(Kind::Claude);
        let url = authorize_url(&profile, Kind::Claude, "the-state", "the-challenge");

        assert!(
            url.starts_with("https://claude.ai/oauth/authorize?"),
            "{url}"
        );
        let q = query_of(&url);
        assert_eq!(q["client_id"], ANTHROPIC_CLIENT_ID);
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(q["code_challenge"], "the-challenge");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "the-state");
        assert!(q["scope"].contains("user:inference"), "{}", q["scope"]);
    }

    /// ChatGPT 側は独自パラメータが要る。offline_access が無いと更新できない。
    #[test]
    fn authorize_url_carries_codex_extras() {
        let profile = auth_profile_for(Kind::Codex);
        let url = authorize_url(&profile, Kind::Codex, "s", "c");

        let q = query_of(&url);
        assert_eq!(q["client_id"], OPENAI_CLIENT_ID);
        assert_eq!(q["redirect_uri"], "http://localhost:1455/auth/callback");
        for scope in [
            "openid",
            "profile",
            "email",
            "offline_access",
            "api.connectors.read",
            "api.connectors.invoke",
        ] {
            assert!(q["scope"].split_whitespace().any(|value| value == scope));
        }
        assert_eq!(q["id_token_add_organizations"], "true");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(q["originator"], "codex_cli_rs");
    }

    #[test]
    fn callback_yields_the_code() {
        assert_eq!(
            code_from_query(Some("code=abc&state=xyz"), "xyz"),
            Ok("abc".to_owned())
        );
    }

    /// state が違う戻りは受け取らない。受け取ると他人の認可結果を掴む。
    #[test]
    fn callback_rejects_mismatched_state() {
        for query in [
            Some("code=abc&state=other"),
            Some("code=abc"), // state 無し
            None,
        ] {
            let err = code_from_query(query, "xyz").unwrap_err();
            assert!(err.contains("state"), "{query:?} → {err}");
        }
    }

    #[test]
    fn callback_without_code_is_an_error() {
        for query in ["state=xyz", "state=xyz&code="] {
            let err = code_from_query(Some(query), "xyz").unwrap_err();
            assert!(err.contains("認可コード"), "{query} → {err}");
        }
    }

    /// 断られた理由をそのまま伝える (何を直せばよいか分かるように)。
    #[test]
    fn callback_error_is_explained() {
        let err = code_from_query(
            Some("error=access_denied&error_description=User+refused&state=xyz"),
            "xyz",
        )
        .unwrap_err();
        assert!(err.contains("User refused"), "{err}");
    }

    /// error があるなら state を見る前に断る (state 無しでも理由を出す)。
    #[test]
    fn callback_error_wins_over_state_check() {
        let err = code_from_query(Some("error=access_denied"), "xyz").unwrap_err();
        assert!(err.contains("access_denied"), "{err}");
    }

    /// upstream 由来の文言をそのまま埋め込まない。
    #[test]
    fn result_page_escapes_upstream_text() {
        let page = result_page(&Err("<script>alert(1)</script>".to_owned()));
        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
    }

    /// ChatGPT のアカウント識別子は id_token の中にしかない。
    #[test]
    fn reads_account_id_from_id_token() {
        let payload = B64URL.encode(
            br#"{"email":"someone@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}"#,
        );
        let claims = jwt_claims(&format!("header.{payload}.signature")).unwrap();

        assert_eq!(account_id_of(&claims).as_deref(), Some("acc-1"));
        assert_eq!(email_of(&claims).as_deref(), Some("someone@example.com"));
    }

    /// 独自 claim が無く、素の項目に入っている形も読む。
    #[test]
    fn reads_account_id_from_top_level_claim() {
        let payload = B64URL.encode(br#"{"chatgpt_account_id":"acc-2"}"#);
        let claims = jwt_claims(&format!("h.{payload}.s")).unwrap();
        assert_eq!(account_id_of(&claims).as_deref(), Some("acc-2"));
    }

    /// 読めない id_token で落ちない (無かった扱いにする)。
    #[test]
    fn broken_id_token_is_ignored() {
        for jwt in ["", "not-a-jwt", "h.!!!.s", "h.aGVsbG8.s"] {
            assert!(jwt_claims(jwt).is_none(), "{jwt}");
        }
    }

    fn exchange_response(raw: &str) -> Result<Tokens> {
        tokens_from(Kind::Claude, serde_json::from_str(raw).unwrap())
    }

    #[test]
    fn exchange_response_becomes_tokens() {
        let t = exchange_response(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":28800,
                "account":{"email_address":"someone@example.com"}}"#,
        )
        .unwrap();

        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token, "rt");
        assert_eq!(t.expires_in, 28_800);
        assert_eq!(t.email.as_deref(), Some("someone@example.com"));
        assert!(t.account_id.is_none(), "Claude 側は account_id を持たない");
    }

    /// refresh token が無いと 8 時間で使えなくなる。保存する前に止める。
    #[test]
    fn exchange_without_refresh_token_fails() {
        let err = exchange_response(r#"{"access_token":"at","expires_in":3600}"#).unwrap_err();
        assert!(err.to_string().contains("refresh token"), "{err}");
    }

    /// expires_in が無ければ期限切れ扱い。初回の使用で更新が走って回復する。
    #[test]
    fn missing_expires_in_falls_back_to_expired() {
        let t = exchange_response(r#"{"access_token":"at","refresh_token":"rt"}"#).unwrap();
        assert_eq!(t.expires_in, 0);
    }

    /// 2026-07-27T19:00:00Z
    const NOW: i64 = 1_785_178_800;

    fn tokens() -> Tokens {
        Tokens {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: None,
            expires_in: 28_800,
            email: Some("someone@example.com".into()),
            account_id: None,
        }
    }

    #[test]
    fn stored_credential_is_built_from_tokens() {
        let c = tokens().to_stored_at(Kind::Claude, None, NOW);

        assert_eq!(c.payload.type_name(), "claude_oauth");
        assert_eq!(c.payload.secret(), "at");
        assert_eq!(c.payload.refresh_token(), Some("rt"));
        assert_eq!(c.payload.email(), "someone@example.com");
        assert_eq!(
            c.payload.expired(),
            "2026-07-28T03:00:00Z",
            "now + expires_in"
        );
        assert_eq!(c.last_refresh, "2026-07-27T19:00:00Z");
        assert_eq!(c.denied_beta_expires_ms, 86_400_000, "既定が入る");
    }

    /// ChatGPT の識別子は codex 側にだけ入る。
    #[test]
    fn codex_credential_carries_the_account_id() {
        let c = Tokens {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: None,
            expires_in: 28_800,
            email: None,
            account_id: Some("acc-1".into()),
        }
        .to_stored_at(Kind::Codex, None, NOW);

        assert_eq!(c.payload.type_name(), "codex_oauth");
        assert_eq!(c.payload.account_id(), Some("acc-1"));
    }

    /// 再ログインで運用側の設定を消さない。
    ///
    /// 消すと、除外リストで組んでいたモデルの割り当てが login のたびに崩れる。
    #[test]
    fn relogin_keeps_operational_settings() {
        let mut base = tokens().to_stored_at(Kind::Claude, None, NOW);
        base.priority = 20;
        base.disabled = true;
        base.excluded_models = vec!["claude-opus-*".to_owned()];
        base.denied_beta_expires_ms = 3_600_000;
        base.record_denied_beta(&["advisor-tool-2026-03-01".to_owned()], NOW);

        let next = Tokens {
            access_token: "at-2".into(),
            refresh_token: "rt-2".into(),
            id_token: None,
            expires_in: 28_800,
            email: None,
            account_id: None,
        }
        .to_stored_at(Kind::Claude, Some(&base), NOW + 60);

        assert_eq!(next.payload.secret(), "at-2", "token は入れ替わる");
        assert_eq!(next.priority, 20);
        assert!(next.disabled);
        assert_eq!(next.excluded_models, vec!["claude-opus-*"]);
        assert_eq!(next.denied_beta_expires_ms, 3_600_000);
        assert!(
            next.denied_beta.contains_key("advisor-tool-2026-03-01"),
            "upstream の観測結果は認可と無関係なので残す"
        );
        assert_eq!(
            next.payload.email(),
            "someone@example.com",
            "分からなかった項目で既存の値を消さない"
        );
    }

    /// payload は取り直したもので置き換える。
    ///
    /// 前回の応答に混ざっていた項目を引きずると、今どのプロバイダから何を
    /// 受け取ったのかが分からなくなる。
    #[test]
    fn relogin_replaces_the_payload() {
        let mut base = tokens().to_stored_at(Kind::Claude, None, NOW);
        if let Some(t) = base.payload.oauth_tokens_mut() {
            t.extra
                .insert("id_token".to_owned(), Value::String("old".to_owned()));
        }

        let next = tokens().to_stored_at(Kind::Claude, Some(&base), NOW + 60);
        let json = serde_json::to_value(&next).unwrap();
        assert!(
            json["payload"].get("id_token").is_none(),
            "前回の残骸を持ち回らない: {json}"
        );
    }

    /// 交換の要求は JSON。更新と同じ形で送る。
    #[test]
    fn exchange_request_is_json() {
        let body = ExchangeRequest {
            client_id: "cid",
            grant_type: "authorization_code",
            code: "the-code",
            redirect_uri: "http://localhost:54545/callback",
            code_verifier: "the-verifier",
            state: Some("the-state"),
        };
        let v: Value = serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();

        assert_eq!(v["grant_type"], "authorization_code");
        assert_eq!(v["code"], "the-code");
        assert_eq!(v["code_verifier"], "the-verifier");
        assert_eq!(v["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(v["state"], "the-state");
    }

    /// 交換の失敗は、やり直せるものと待つべきものを言い分ける。
    #[test]
    fn exchange_failure_is_actionable() {
        assert!(exchange_failure_reason(400, "").contains("やり直"));
        assert!(exchange_failure_reason(403, "").contains("利用権"));
        assert!(exchange_failure_reason(429, "").contains("待って"));
        assert!(exchange_failure_reason(503, "").contains("一時的"));
        assert!(exchange_failure_reason(418, "").contains("418"));
    }

    /// upstream が理由を言っているなら、それを添えて出す。
    /// 状態からの言い換えだけだと、何が違うのか分からず推測で直すことになる。
    #[test]
    fn exchange_failure_carries_upstream_reason() {
        let body = r#"{"error":"invalid_grant","error_description":"code is expired"}"#;
        let reason = exchange_failure_reason(400, body);
        assert!(reason.contains("invalid_grant"), "{reason}");
        assert!(reason.contains("code is expired"), "{reason}");
    }

    /// 失敗応答が OAuth の形でなければ、状態からの言い換えだけを出す。
    #[test]
    fn exchange_failure_without_oauth_shape_is_plain() {
        let reason = exchange_failure_reason(400, "<html>gateway error</html>");
        assert!(reason.contains("やり直"));
        assert!(!reason.contains("応答:"), "{reason}");
    }

    /// 確認先が断ったら、次に何をすればよいかが分かる文言になる。
    #[test]
    fn verify_failure_is_actionable() {
        assert!(verify_failure_reason(401).contains("利用権"));
        assert!(verify_failure_reason(403).contains("やり直"));
        assert!(verify_failure_reason(429).contains("待って"));
        assert!(verify_failure_reason(503).contains("一時的"));
        assert!(verify_failure_reason(418).contains("418"));
    }

    /// 交換の応答。
    const TOKENS: &str = r#"{"access_token":"at","refresh_token":"rt","expires_in":28800,
        "account":{"email_address":"someone@example.com"}}"#;

    /// 確認の応答 (Claude はモデル一覧が返る)。
    const MODELS: &str = r#"{"data":[{"id":"claude-opus-5","created_at":"2026-07-24T00:00:00Z"}]}"#;

    type Canned = (axum::http::StatusCode, &'static str);

    fn ok(body: &'static str) -> Canned {
        (axum::http::StatusCode::OK, body)
    }

    /// 交換先と確認先を装うサーバ。決め打ちの応答を返す。
    struct Upstream {
        exchange_url: String,
        verify_url: String,
        /// 確認に入ったことの知らせ。止めていないときは鳴らない。
        entered_verify: Arc<tokio::sync::Notify>,
        /// 確認を進ませる合図。
        release_verify: Arc<tokio::sync::Notify>,
    }

    impl Upstream {
        async fn start(exchange: Canned, verify: Canned, gated: bool) -> Self {
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let (entered_verify, release_verify) = (Arc::clone(&entered), Arc::clone(&release));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = axum::Router::new()
                .route(
                    "/token",
                    axum::routing::post(move || async move { exchange }),
                )
                .route(
                    "/verify",
                    axum::routing::get(move || async move {
                        if gated {
                            entered.notify_one();
                            release.notified().await;
                        }
                        verify
                    }),
                );
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            Self {
                exchange_url: format!("http://{addr}/token"),
                verify_url: format!("http://{addr}/verify"),
                entered_verify,
                release_verify,
            }
        }

        /// 素直に通るサーバ。
        async fn healthy() -> Self {
            Self::start(ok(TOKENS), ok(MODELS), false).await
        }

        fn endpoints(&self) -> Endpoints<'_> {
            Endpoints {
                exchange: Some(&self.exchange_url),
                verify: Some(&self.verify_url),
            }
        }
    }

    /// ブラウザの代わりに戻り先を叩き、返ってきた画面を受け取る。
    ///
    /// 画面は交換・確認・保存が終わるまで返らないので、`finish` と
    /// 同時に走らせて後から拾う。
    fn visit_callback(
        port: u16,
        path: &str,
        params: &[(&str, &str)],
    ) -> tokio::task::JoinHandle<String> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().copied())
            .finish();
        let url = format!("http://127.0.0.1:{port}{path}?{query}");
        tokio::spawn(async move {
            match reqwest::get(url).await {
                Ok(resp) => resp.text().await.unwrap_or_default(),
                Err(e) => panic!("戻り先を叩けません: {e}"),
            }
        })
    }

    async fn start_login(kind: Kind) -> (Authorization, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (begin_on(kind, auth_profile_for(kind), listener), port)
    }

    /// 保存された token。呼ばれたかどうかも見る。
    type Saved = std::cell::RefCell<Option<String>>;

    fn save_into(saved: &Saved) -> impl FnOnce(&Tokens) -> Result<String> + '_ {
        move |tokens| {
            *saved.borrow_mut() = Some(tokens.access_token.clone());
            Ok(tokens.access_token.clone())
        }
    }

    /// 認可 URL を出す → 戻りを受ける → 交換 → 確認 → 保存、が通しで動く。
    ///
    /// 成功の画面が出るのは全部済んだ後だけ。
    #[tokio::test]
    async fn login_exchanges_verifies_and_saves() {
        let upstream = Upstream::healthy().await;
        let (auth, port) = start_login(Kind::Claude).await;
        let state = query_of(auth.url())["state"].clone();
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "the-code"), ("state", &state)],
        );
        let token = auth
            .finish_with(upstream.endpoints(), save_into(&saved))
            .await
            .unwrap();

        assert_eq!(token, "at");
        assert_eq!(saved.into_inner().as_deref(), Some("at"), "保存まで進む");

        let page = page.await.unwrap();
        assert!(page.contains("認可できました"), "{page}");
        assert!(page.contains("確認して保存"), "{page}");
    }

    /// 画面に認可コードも token も出さない (履歴や共有画面に残る)。
    #[tokio::test]
    async fn success_page_shows_no_secrets() {
        let upstream = Upstream::healthy().await;
        let (auth, port) = start_login(Kind::Claude).await;
        let state = query_of(auth.url())["state"].clone();
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "secret-code-value"), ("state", &state)],
        );
        auth.finish_with(upstream.endpoints(), save_into(&saved))
            .await
            .unwrap();

        let page = page.await.unwrap();
        assert!(!page.contains("secret-code-value"), "{page}");
    }

    /// ChatGPT 側は id_token から account_id まで拾えて初めて使える。
    #[tokio::test]
    async fn codex_login_picks_up_the_account_id() {
        // {"email":"someone@example.com",
        //  "https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}
        let upstream = Upstream::start(
            ok(r#"{"access_token":"at","refresh_token":"rt","expires_in":864000,
                "id_token":"h.eyJlbWFpbCI6InNvbWVvbmVAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEifX0.s"}"#),
            // Codex の確認先はモデル一覧を返さない。通ることだけを見る。
            ok(r#"{"email":"someone@example.com"}"#),
            false,
        )
        .await;
        let (auth, port) = start_login(Kind::Codex).await;
        let state = query_of(auth.url())["state"].clone();

        let page = visit_callback(
            port,
            "/auth/callback",
            &[("code", "the-code"), ("state", &state)],
        );
        let account = auth
            .finish_with(upstream.endpoints(), |tokens| {
                assert_eq!(tokens.email.as_deref(), Some("someone@example.com"));
                assert_eq!(tokens.expires_in, 864_000);
                Ok(tokens.account_id.clone())
            })
            .await
            .unwrap();

        assert_eq!(account.as_deref(), Some("acc-1"));
        assert!(page.await.unwrap().contains("認可できました"));
    }

    /// state の違う戻りは受け取らず、交換にも進まない。
    #[tokio::test]
    async fn login_refuses_a_foreign_callback() {
        let (auth, port) = start_login(Kind::Claude).await;
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "the-code"), ("state", "someone-elses-state")],
        );
        // 交換先も確認先も潰しておく。ここへ進んだら state の検査が効いていない。
        let endpoints = Endpoints {
            exchange: Some("http://127.0.0.1:1/token"),
            verify: Some("http://127.0.0.1:1/verify"),
        };
        let err = auth
            .finish_with(endpoints, save_into(&saved))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("state"), "{err}");
        assert!(saved.into_inner().is_none(), "保存まで進まない");
        assert!(page.await.unwrap().contains("認可できませんでした"));
    }

    /// 断られた戻りも、理由付きで終わる (無言で待ち続けない)。
    #[tokio::test]
    async fn login_reports_a_denied_authorization() {
        let (auth, port) = start_login(Kind::Claude).await;
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("error", "access_denied"), ("state", &state_of(&auth))],
        );
        let err = auth
            .finish_with(Endpoints::default(), save_into(&saved))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("access_denied"), "{err}");
        assert!(page.await.unwrap().contains("access_denied"));
    }

    fn state_of(auth: &Authorization) -> String {
        query_of(auth.url())["state"].clone()
    }

    /// 交換に失敗したら、画面も失敗になる。保存もしない。
    ///
    /// 交換の失敗は端末にしか出ない、では足りない。人が見ているのは
    /// ブラウザの画面のほう。
    #[tokio::test]
    async fn exchange_failure_shows_a_failure_page() {
        let upstream = Upstream::start(
            (
                axum::http::StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant","access_token":"leaked-secret-value"}"#,
            ),
            ok(MODELS),
            false,
        )
        .await;
        let (auth, port) = start_login(Kind::Claude).await;
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "c"), ("state", &state_of(&auth))],
        );
        let err = auth
            .finish_with(upstream.endpoints(), save_into(&saved))
            .await
            .unwrap_err()
            .to_string();

        assert!(!err.contains("leaked-secret-value"), "{err}");
        assert!(err.contains("やり直"), "{err}");
        assert!(
            saved.into_inner().is_none(),
            "交換できていないので保存しない"
        );

        let page = page.await.unwrap();
        assert!(page.contains("認可できませんでした"), "{page}");
        assert!(page.contains("やり直"), "{page}");
        assert!(!page.contains("leaked-secret-value"), "{page}");
    }

    /// 確認に失敗したら、画面も失敗になる。保存もしない。
    ///
    /// 交換だけ通って使えない token を保存すると、次の転送まで気づけない。
    #[tokio::test]
    async fn verify_failure_shows_a_failure_page() {
        let upstream = Upstream::start(
            ok(TOKENS),
            (
                axum::http::StatusCode::UNAUTHORIZED,
                r#"{"error":{"message":"Invalid bearer token"}}"#,
            ),
            false,
        )
        .await;
        let (auth, port) = start_login(Kind::Claude).await;
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "c"), ("state", &state_of(&auth))],
        );
        let err = auth
            .finish_with(upstream.endpoints(), save_into(&saved))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("受け付けられませんでした"), "{err}");
        assert!(saved.into_inner().is_none(), "使えない token は保存しない");
        assert!(page.await.unwrap().contains("認可できませんでした"));
    }

    /// モデルが 1 つも見えない token も、使えないものとして断る。
    #[tokio::test]
    async fn verify_rejects_an_empty_model_list() {
        let upstream = Upstream::start(ok(TOKENS), ok(r#"{"data":[]}"#), false).await;
        let (auth, port) = start_login(Kind::Claude).await;
        let saved = Saved::default();

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "c"), ("state", &state_of(&auth))],
        );
        let err = auth
            .finish_with(upstream.endpoints(), save_into(&saved))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("モデルが 1 つもありません"), "{err}");
        assert!(saved.into_inner().is_none());
        assert!(page.await.unwrap().contains("認可できませんでした"));
    }

    /// 保存に失敗したら、画面も失敗になる。
    #[tokio::test]
    async fn save_failure_shows_a_failure_page() {
        let upstream = Upstream::healthy().await;
        let (auth, port) = start_login(Kind::Claude).await;

        let page = visit_callback(
            port,
            "/callback",
            &[("code", "c"), ("state", &state_of(&auth))],
        );
        let err = auth
            .finish_with(upstream.endpoints(), |_| -> Result<()> {
                Err(Error::Login {
                    reason: "置き場に書き込めません".to_owned(),
                })
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("置き場に書き込めません"), "{err}");

        let page = page.await.unwrap();
        assert!(page.contains("認可できませんでした"), "{page}");
        assert!(page.contains("置き場に書き込めません"), "{page}");
    }

    /// 交換の途中で 2 本目の戻りが来ても、1 本目の処理を壊さない。
    ///
    /// 2 本目には結果を渡さない (渡す先が 1 つしかない)。
    #[tokio::test]
    async fn a_second_callback_does_not_disturb_the_first() {
        let upstream = Upstream::start(ok(TOKENS), ok(MODELS), true).await;
        let (auth, port) = start_login(Kind::Claude).await;
        let state = state_of(&auth);
        let saved = Saved::default();

        let first = visit_callback(port, "/callback", &[("code", "c"), ("state", &state)]);
        let entered = Arc::clone(&upstream.entered_verify);
        let release = Arc::clone(&upstream.release_verify);

        let finished = async {
            // 1 本目が確認まで進んでから 2 本目を送る。
            entered.notified().await;
            let second =
                visit_callback(port, "/callback", &[("code", "c2"), ("state", &state)]).await;
            release.notify_one();
            second.unwrap()
        };

        let (result, second) = tokio::join!(
            auth.finish_with(upstream.endpoints(), save_into(&saved)),
            finished
        );

        assert_eq!(result.unwrap(), "at", "1 本目はそのまま通る");
        assert!(second.contains("受け付け済み"), "{second}");
        assert!(first.await.unwrap().contains("認可できました"));
    }
}
