//! ChatGPT サブスク経路の request-time 認証。

use crate::credential::Credential;
use crate::egress::UpstreamRequest;
use crate::provider::Auth;
use crate::{Error, Result};

/// access token と ChatGPT account ID を載せる。
pub struct ChatGptBearer {
    route: String,
}

impl ChatGptBearer {
    pub fn new(route: impl Into<String>) -> Self {
        Self {
            route: route.into(),
        }
    }
}

impl Auth for ChatGptBearer {
    fn authorize(
        &self,
        credential: Option<&Credential>,
        request: &mut UpstreamRequest,
    ) -> Result<()> {
        let credential = credential.ok_or_else(|| Error::Credential {
            id: self.route.clone(),
            reason: "ChatGPT の認証情報が渡されていません".to_owned(),
        })?;
        let account_id = credential
            .account_id
            .as_deref()
            .ok_or_else(|| Error::Credential {
                id: credential.id.to_string(),
                reason: "ChatGPT のアカウント識別子がありません。login をやり直してください"
                    .to_owned(),
            })?;
        request.headers.set("authorization", credential.bearer());
        request.headers.set("chatgpt-account-id", account_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::Headers;
    use bytes::Bytes;

    fn request() -> UpstreamRequest {
        UpstreamRequest {
            url: "https://chatgpt.com/backend-api/codex/responses".to_owned(),
            headers: Headers::default(),
            body: Bytes::from_static(b"{}"),
        }
    }

    /// ChatGPT backend は token と account ID の両方を要求する。
    #[test]
    fn puts_both_auth_headers_on_the_request() {
        let mut credential = Credential::for_test("tok");
        credential.account_id = Some("acc-1".to_owned());
        let mut request = request();

        ChatGptBearer::new("codex")
            .authorize(Some(&credential), &mut request)
            .unwrap();

        assert_eq!(request.headers.get("authorization"), Some("Bearer tok"));
        assert_eq!(request.headers.get("chatgpt-account-id"), Some("acc-1"));
    }

    /// account ID が無い token を送って 401 にするより、送信前に再 login を案内する。
    #[test]
    fn missing_account_id_is_actionable() {
        let error = ChatGptBearer::new("codex")
            .authorize(Some(&Credential::for_test("tok")), &mut request())
            .unwrap_err();
        assert!(error.to_string().contains("login"), "{error}");
    }
}
