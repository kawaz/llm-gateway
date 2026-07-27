//! OAuth token の更新。
//!
//! Anthropic と ChatGPT で endpoint と client_id が違うだけで、手順は同じ:
//! `refresh_token` を送ると新しい access token が返り、**refresh token 自体も
//! 入れ替わる**。返ってきた値を保存し損ねると、次の更新で弾かれて
//! 再ログインが要る状態になる。

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

use super::{CredentialId, Kind};

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
}
