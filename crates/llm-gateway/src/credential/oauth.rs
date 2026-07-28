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
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 有効期間 (秒)。Anthropic は 8 時間、ChatGPT は 10 日を返す。
    pub expires_in: i64,
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

/// 応答を返し切るのを待つ上限。ここを待たずに閉じるとブラウザに白紙が出る。
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
            scope: "openid profile email offline_access",
            // 独自パラメータ。id_token に組織情報を載せさせ、Codex CLI 用の
            // 簡易フローに乗る。originator は ChatGPT 経路が要求する値と同じ
            // (DR-0002 の接続仕様)。
            extra_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "codex_cli_rs"),
            ],
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

    /// 戻りを待ち、受け取った認可コードを token に交換する。
    pub async fn finish(self) -> Result<Tokens> {
        self.finish_at(None).await
    }

    /// 交換先を指定して仕上げる。
    ///
    /// `url_override` は試験用。本番は [`Authorization::finish`] を使い、
    /// 種別から決まる交換先へ送る。
    pub async fn finish_at(self, url_override: Option<&str>) -> Result<Tokens> {
        let code =
            wait_for_callback(self.listener, self.profile.redirect_path, &self.state).await?;
        exchange_code(
            self.kind,
            &self.profile,
            &code,
            &self.state,
            &self.verifier,
            url_override,
        )
        .await
    }
}

/// 戻り先を待ち受けるアドレス。
///
/// 既定はループバックのみ。`LLM_GATEWAY_LOGIN_BIND` で上書きできる。
///
/// 手元のブラウザと gateway が別マシンにある時 (リモート作業) は
/// ループバックに戻ってこられないので、tailnet のアドレスを指定して直接
/// 受ける。`0.0.0.0` にすれば全インタフェースで待つが、認可コードを
/// 受ける口を広く晒すことになるので、必要な 1 つに絞るのが望ましい。
fn callback_bind_addr() -> String {
    std::env::var("LLM_GATEWAY_LOGIN_BIND").unwrap_or_else(|_| "127.0.0.1".to_owned())
}

/// 認可を始める。戻り先を開き、ブラウザで開く URL を組み立てる。
pub async fn begin(kind: Kind) -> Result<Authorization> {
    let profile = auth_profile_for(kind);
    let host = callback_bind_addr();
    let listener = tokio::net::TcpListener::bind((host.as_str(), profile.redirect_port))
        .await
        .map_err(|e| Error::Login {
            reason: format!(
                "{host}:{} で待ち受けられません: {e}。\
戻り先のポートは client_id に登録済みの値なので他では代用できません。\
このポートを使っている別のログイン処理 (cpa など) を止めるか、\
LLM_GATEWAY_LOGIN_BIND で待ち受けるアドレスを指定してから、もう一度実行してください",
                profile.redirect_port
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

/// ブラウザからの戻りを 1 回だけ受け、認可コードを返す。
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    path: &str,
    expected_state: &str,
) -> Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let done = Arc::new(tokio::sync::Notify::new());
    let state = Arc::new(CallbackState {
        expected: expected_state.to_owned(),
        sender: tokio::sync::Mutex::new(Some(tx)),
        done: Arc::clone(&done),
    });

    let app = axum::Router::new()
        .route(path, axum::routing::get(receive))
        .with_state(state);
    // 受け取ったら閉じる。graceful なので、返事を返し切ってから終わる。
    let serving =
        axum::serve(listener, app).with_graceful_shutdown(async move { done.notified().await });
    let mut server = tokio::spawn(async move { serving.await });

    // 受け口ごと倒れたら送り口も落ちるので、待ちっぱなしにはならない。
    // 受け口の生死を別に見張る必要はない。
    let outcome = match tokio::time::timeout(CALLBACK_TIMEOUT, rx).await {
        Ok(received) => received.unwrap_or_else(|_| Err("認可の受け口が落ちました".to_owned())),
        Err(_) => Err(format!(
            "{} 秒待っても戻ってきませんでした。\
ブラウザで許可まで進めてから、もう一度実行してください",
            CALLBACK_TIMEOUT.as_secs()
        )),
    };

    // 受け取れた場合も断られた場合も handler が閉じる合図を出しているので、
    // ここで待つと画面に結果を出し切ってから終わる。時間切れのときだけ自分で止める。
    if tokio::time::timeout(FLUSH_TIMEOUT, &mut server)
        .await
        .is_err()
    {
        server.abort();
    }

    outcome.map_err(|reason| Error::Login { reason })
}

/// 戻りを 1 つだけ受け取るための置き場。
struct CallbackState {
    expected: String,
    /// 1 回で使い切る。2 回目以降の要求には結果を渡さない。
    sender: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<CallbackOutcome>>>,
    done: Arc<tokio::sync::Notify>,
}

type CallbackOutcome = std::result::Result<String, String>;

/// ブラウザからの戻りを受ける。
async fn receive(
    axum::extract::State(state): axum::extract::State<Arc<CallbackState>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> axum::response::Html<String> {
    let outcome = code_from_query(query.as_deref(), &state.expected);
    let page = result_page(&outcome);

    if let Some(tx) = state.sender.lock().await.take() {
        let _ = tx.send(outcome);
    }
    state.done.notify_one();

    axum::response::Html(page)
}

/// 戻りの query から認可コードを取り出す。
///
/// state が一致しない戻りは捨てる。一致を見ないと、別の誰かに始めさせた認可の
/// 結果を掴まされる (CSRF)。掴んだ token は相手のアカウントのものになる。
fn code_from_query(query: Option<&str>, expected_state: &str) -> CallbackOutcome {
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
/// 認可コードは出さない。画面はブラウザの履歴や共有画面に残る。
fn result_page(outcome: &CallbackOutcome) -> String {
    let body = match outcome {
        Ok(_) => {
            "<h1>認可できました</h1><p>この画面は閉じて、端末に戻ってください。</p>".to_owned()
        }
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
    }
}

impl Tokens {
    /// 保存する形にする。
    pub fn to_stored(&self, kind: Kind, base: Option<&StoredCredential>) -> StoredCredential {
        self.to_stored_at(kind, base, now_unix())
    }

    /// 時刻を指定して保存する形にする。`now_unix` は試験用。
    ///
    /// `base` は再ログイン時の既存の内容。priority / disabled / 除外リストは
    /// 認可で決まるものではなく運用側の設定なので、上書きせず引き継ぐ
    /// (消すと、再ログインのたびにモデルの割り当てが崩れる)。
    pub fn to_stored_at(
        &self,
        kind: Kind,
        base: Option<&StoredCredential>,
        now_unix: i64,
    ) -> StoredCredential {
        let mut next = match base {
            Some(base) => base.clone(),
            None => StoredCredential {
                kind,
                email: String::new(),
                access_token: String::new(),
                refresh_token: String::new(),
                expired: String::new(),
                last_refresh: String::new(),
                priority: 0,
                disabled: false,
                excluded_models: Vec::new(),
                account_id: None,
                extra: BTreeMap::new(),
            },
        };

        next.kind = kind;
        next.access_token = self.access_token.clone();
        next.refresh_token = self.refresh_token.clone();
        next.expired = crate::credential::store::format_rfc3339(now_unix + self.expires_in);
        next.last_refresh = crate::credential::store::format_rfc3339(now_unix);
        // 分からなかった項目で既存の値を消さない。
        if let Some(email) = &self.email {
            next.email = email.clone();
        }
        if self.account_id.is_some() {
            next.account_id = self.account_id.clone();
        }
        next
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 交換の要求。
///
/// `state` は Anthropic が交換時にも要求する。ChatGPT 側は見ないが、
/// 認可サーバは知らないパラメータを無視する規定 (RFC 6749 §3.2) なので
/// 経路を分けず、常に送る。
#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
    state: &'a str,
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
        state,
    };

    // 更新と違い、ここでは共有のクライアントを持ち回らない (login は 1 回で終わる)。
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Login {
            reason: format!("HTTP クライアントを作れません: {e}"),
        })?;

    let resp = http
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Login {
            reason: format!("{url} に接続できません: {e}"),
        })?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| Error::Login {
        reason: format!("応答を読めません: {e}"),
    })?;

    if !status.is_success() {
        return Err(Error::Login {
            reason: exchange_failure_reason(status.as_u16()),
        });
    }

    let parsed: ExchangeResponse = serde_json::from_str(&text).map_err(|e| Error::Login {
        reason: format!("応答の形式が想定と違います: {e}"),
    })?;
    tokens_from(parsed)
}

/// 応答を保存できる形にする。
fn tokens_from(resp: ExchangeResponse) -> Result<Tokens> {
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

    Ok(Tokens {
        access_token: resp.access_token,
        refresh_token,
        // 返らなかった場合は期限切れ扱いにする。初回の使用で更新が走るだけで
        // 済み、認可をやり直させずに回復できる。
        expires_in: resp.expires_in.unwrap_or(0),
        email,
        account_id: claims.as_ref().and_then(account_id_of),
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
fn exchange_failure_reason(status: u16) -> String {
    match status {
        400 => "認可コードが受け付けられませんでした。\
使い切ったか期限切れの可能性があります。もう一度 login をやり直してください"
            .to_owned(),
        401 | 403 => "認可が拒否されました。\
ログインしたアカウントにこのサブスクの利用権があるか確認してください"
            .to_owned(),
        429 => "要求が多すぎます。しばらく待ってから再試行してください".to_owned(),
        500..=599 => format!("交換先が {status} を返しました。一時的な障害の可能性があります"),
        other => format!("交換先が {other} を返しました"),
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
        assert_eq!(r.access_token, "new-at");
        assert_eq!(r.refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(r.expires_in, 28_800, "Anthropic は 8 時間");
        assert_eq!(
            r.account.and_then(|a| a.email_address).as_deref(),
            Some("someone@example.com")
        );
    }

    /// refresh_token が返らない場合もある。その時は入れ替わっていない扱い。
    #[test]
    fn refresh_token_may_be_absent() {
        let raw = r#"{"access_token": "new-at", "expires_in": 3600}"#;
        let r: RefreshResponse = serde_json::from_str(raw).unwrap();
        assert!(r.refresh_token.is_none());
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
        assert!(q["scope"].contains("offline_access"), "{}", q["scope"]);
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

    /// 画面に認可コードを出さない (履歴や共有画面に残る)。
    #[test]
    fn result_page_does_not_show_the_code() {
        let page = result_page(&Ok("secret-code-value".to_owned()));
        assert!(!page.contains("secret-code-value"), "{page}");
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
        tokens_from(serde_json::from_str(raw).unwrap())
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
            expires_in: 28_800,
            email: Some("someone@example.com".into()),
            account_id: None,
        }
    }

    #[test]
    fn stored_credential_is_built_from_tokens() {
        let c = tokens().to_stored_at(Kind::Claude, None, NOW);

        assert_eq!(c.kind, Kind::Claude);
        assert_eq!(c.access_token, "at");
        assert_eq!(c.refresh_token, "rt");
        assert_eq!(c.email, "someone@example.com");
        assert_eq!(c.expired, "2026-07-28T03:00:00Z", "now + expires_in");
        assert_eq!(c.last_refresh, "2026-07-27T19:00:00Z");
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
        base.extra
            .insert("id_token".to_owned(), Value::String(String::new()));

        let next = Tokens {
            access_token: "at-2".into(),
            refresh_token: "rt-2".into(),
            expires_in: 28_800,
            email: None,
            account_id: None,
        }
        .to_stored_at(Kind::Claude, Some(&base), NOW + 60);

        assert_eq!(next.access_token, "at-2", "token は入れ替わる");
        assert_eq!(next.priority, 20);
        assert!(next.disabled);
        assert_eq!(next.excluded_models, vec!["claude-opus-*"]);
        assert!(next.extra.contains_key("id_token"), "未知の項目も残す");
        assert_eq!(
            next.email, "someone@example.com",
            "分からなかった項目で既存の値を消さない"
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
            state: "the-state",
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
        assert!(exchange_failure_reason(400).contains("やり直"));
        assert!(exchange_failure_reason(403).contains("利用権"));
        assert!(exchange_failure_reason(429).contains("待って"));
        assert!(exchange_failure_reason(503).contains("一時的"));
        assert!(exchange_failure_reason(418).contains("418"));
    }

    /// 交換先を装うサーバ。1 本受けたら決め打ちの応答を返す。
    async fn fake_token_server(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app =
            axum::Router::new().route("/token", axum::routing::post(move || async move { body }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/token")
    }

    /// ブラウザの代わりに戻り先を叩く。
    fn visit_callback(port: u16, path: &str, params: &[(&str, &str)]) {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().copied())
            .finish();
        let url = format!("http://127.0.0.1:{port}{path}?{query}");
        tokio::spawn(async move {
            let _ = reqwest::get(url).await;
        });
    }

    async fn start_login(kind: Kind) -> (Authorization, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (begin_on(kind, auth_profile_for(kind), listener), port)
    }

    /// 認可 URL を出す → 戻りを受ける → token に交換する、が通しで動く。
    #[tokio::test]
    async fn login_exchanges_the_callback_code() {
        let token_url = fake_token_server(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":28800,
                "account":{"email_address":"someone@example.com"}}"#,
        )
        .await;
        let (auth, port) = start_login(Kind::Claude).await;
        let state = query_of(auth.url())["state"].clone();

        visit_callback(
            port,
            "/callback",
            &[("code", "the-code"), ("state", &state)],
        );
        let tokens = auth.finish_at(Some(&token_url)).await.unwrap();

        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, "rt");
        assert_eq!(tokens.email.as_deref(), Some("someone@example.com"));
    }

    /// ChatGPT 側は id_token から account_id まで拾えて初めて使える。
    #[tokio::test]
    async fn codex_login_picks_up_the_account_id() {
        // {"email":"someone@example.com",
        //  "https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}
        let token_url = fake_token_server(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":864000,
                "id_token":"h.eyJlbWFpbCI6InNvbWVvbmVAZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEifX0.s"}"#,
        )
        .await;
        let (auth, port) = start_login(Kind::Codex).await;
        let state = query_of(auth.url())["state"].clone();

        visit_callback(
            port,
            "/auth/callback",
            &[("code", "the-code"), ("state", &state)],
        );
        let tokens = auth.finish_at(Some(&token_url)).await.unwrap();

        assert_eq!(tokens.account_id.as_deref(), Some("acc-1"));
        assert_eq!(tokens.email.as_deref(), Some("someone@example.com"));
        assert_eq!(tokens.expires_in, 864_000);
    }

    /// state の違う戻りは受け取らず、交換にも進まない。
    #[tokio::test]
    async fn login_refuses_a_foreign_callback() {
        let (auth, port) = start_login(Kind::Claude).await;

        visit_callback(
            port,
            "/callback",
            &[("code", "the-code"), ("state", "someone-elses-state")],
        );
        // 交換先を潰しておく。ここへ進んだら state の検査が効いていない。
        let err = auth
            .finish_at(Some("http://127.0.0.1:1/token"))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("state"), "{err}");
    }

    /// 断られた戻りも、理由付きで終わる (無言で待ち続けない)。
    #[tokio::test]
    async fn login_reports_a_denied_authorization() {
        let (auth, port) = start_login(Kind::Claude).await;
        let state = query_of(auth.url())["state"].clone();

        visit_callback(
            port,
            "/callback",
            &[("error", "access_denied"), ("state", &state)],
        );
        let err = auth.finish_at(None).await.unwrap_err();

        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    /// 交換先が断ったら、応答本文ではなく状態から言い換えて返す。
    #[tokio::test]
    async fn login_reports_exchange_failure_without_the_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":"invalid_grant","access_token":"leaked-secret-value"}"#,
                )
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (auth, port) = start_login(Kind::Claude).await;
        let state = query_of(auth.url())["state"].clone();
        visit_callback(port, "/callback", &[("code", "c"), ("state", &state)]);

        let err = auth
            .finish_at(Some(&format!("http://{addr}/token")))
            .await
            .unwrap_err()
            .to_string();

        assert!(!err.contains("leaked-secret-value"), "{err}");
        assert!(err.contains("やり直"), "{err}");
    }
}
