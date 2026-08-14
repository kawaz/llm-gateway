//! Anthropic 公式の認証。
//!
//! サブスクの OAuth token をそのまま `authorization` に載せるだけ。載せるのは
//! **送る直前**で、token の取得と更新は [`crate::credential`] が持つ。

use crate::credential::Credential;
use crate::egress::{Headers, UpstreamRequest};
use crate::provider::Auth;
use crate::{Error, Result};

/// OAuth の access token を Bearer として載せる。
pub struct OauthBearer {
    /// 認証情報を用意できなかったときに、どの経路の話かを言うための名前。
    route: String,
}

impl OauthBearer {
    pub fn new(route: impl Into<String>) -> Self {
        Self {
            route: route.into(),
        }
    }

    /// ヘッダに認証を載せる。
    pub fn apply(&self, credential: Option<&Credential>, headers: &mut Headers) -> Result<()> {
        let credential = credential.ok_or_else(|| Error::Credential {
            id: self.route.clone(),
            reason: "no credential was provided".to_owned(),
        })?;
        headers.set("authorization", credential.bearer());
        Ok(())
    }
}

impl Auth for OauthBearer {
    fn authorize(
        &self,
        credential: Option<&Credential>,
        request: &mut UpstreamRequest,
    ) -> Result<()> {
        self.apply(credential, &mut request.headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn request() -> UpstreamRequest {
        UpstreamRequest {
            url: "https://api.anthropic.com/v1/messages".to_owned(),
            headers: Headers::default(),
            body: Bytes::from_static(b"{}"),
        }
    }

    #[test]
    fn puts_the_token_in_the_authorization_header() {
        let credential = Credential::for_test("tok");
        let mut request = request();

        OauthBearer::new("claude-personal")
            .authorize(Some(&credential), &mut request)
            .unwrap();

        assert_eq!(request.headers.get("authorization"), Some("Bearer tok"));
        assert_eq!(request.headers.get("x-api-key"), None);
    }

    /// 認証情報が要るのに渡されなければ、送る前に落とす。
    #[test]
    fn missing_credential_is_an_error() {
        let err = OauthBearer::new("claude-personal")
            .authorize(None, &mut request())
            .unwrap_err();
        assert!(err.to_string().contains("claude-personal"), "{err}");
    }

    /// 署名するのはヘッダだけで、直列化済みのボディには触らない。
    #[test]
    fn leaves_the_serialized_body_untouched() {
        let credential = Credential::for_test("tok");
        let mut request = request();
        let before = request.body.clone();

        OauthBearer::new("c")
            .authorize(Some(&credential), &mut request)
            .unwrap();

        assert_eq!(request.body, before);
        assert_eq!(
            request.body.as_ptr(),
            before.as_ptr(),
            "同じ領域を送る (直列化し直さない)"
        );
    }
}
