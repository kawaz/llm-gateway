//! Bedrock の Anthropic 互換 — 認証だけ独自の composite preset (DR-0014 §6)。
//!
//! 公式との違いは 3 つしかない: `x-api-key` で認証する、モデル名が自分の
//! 名前空間、受け付けない beta フラグがある。方言は同じなので
//! [`AnthropicWire`] を**書き直さず**、接続先とモデル ID 体系を設定として
//! 渡すだけで再利用する。
//!
//! 認証の軸と方言の軸が直交している (DR-0004 の 2 軸分離) ことの実証であり、
//! 小 trait 分割が正しく切れているかの試金石でもある — ここで Wire を写経する
//! ことになったら、切り方が間違っている。

use std::sync::Arc;

use crate::credential::Credential;
use crate::egress::{Headers, UpstreamRequest};
use crate::preset::anthropic::{AnthropicMetering, AnthropicWire, beta};
use crate::provider::{Auth, Preset};
use crate::{Error, Result};

/// Bedrock の認証。API キーを `x-api-key` に載せる。
///
/// [`OauthBearer`] と載せる先が違うだけに見えるが、`authorization` で送ると
/// 401 になる (実測)。
pub struct ApiKey {
    /// 認証情報を用意できなかったときに、どの経路の話かを言うための名前。
    route: String,
}

impl ApiKey {
    pub fn new(route: impl Into<String>) -> Self {
        Self {
            route: route.into(),
        }
    }

    /// ヘッダに認証を載せる。
    pub fn apply(&self, credential: Option<&Credential>, headers: &mut Headers) -> Result<()> {
        let credential = credential.ok_or_else(|| Error::Credential {
            id: self.route.clone(),
            reason: "認証情報が渡されていません".to_owned(),
        })?;
        headers.set("x-api-key", credential.api_key());
        Ok(())
    }
}

impl Auth for ApiKey {
    fn authorize(
        &self,
        credential: Option<&Credential>,
        request: &mut UpstreamRequest,
    ) -> Result<()> {
        self.apply(credential, &mut request.headers)
    }
}

/// Bedrock の preset を組む。
///
/// `wire` には Bedrock の接続先とモデル ID 対応表を入れたものを渡す。枠照会
/// API は無いので `None` — 呼んでから「未対応」を返す実装を置かない。
pub fn preset(name: &str, wire: Arc<AnthropicWire>) -> Preset {
    Preset::new(
        name,
        Arc::new(ApiKey::new(name)),
        wire,
        Arc::new(AnthropicMetering),
        None,
    )
}

/// この upstream へ送るときの beta の既定。
///
/// 設定で差し替えられる。差し替えたら既定の一覧は使わない (実測で分かって
/// いる分より、運用者が今見ている upstream の方が新しい)。
pub fn beta_policy(deny: Option<Vec<String>>) -> beta::Policy {
    match deny {
        Some(flags) => beta::Policy::Deny(flags.into_iter().collect()),
        None => beta::Policy::bedrock(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::EgressRequest;
    use crate::preset::anthropic::{self, OauthBearer};
    use crate::provider::Wire;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    const BASE_URL: &str = "https://bedrock-mantle.us-east-1.api.aws/anthropic";

    fn wire() -> Arc<AnthropicWire> {
        Arc::new(AnthropicWire::new(
            "bedrock",
            BASE_URL,
            BTreeMap::new(),
            BTreeMap::from([(
                "claude-fable-5".to_owned(),
                "anthropic.claude-fable-5".to_owned(),
            )]),
        ))
    }

    fn request(model: &str) -> EgressRequest {
        EgressRequest {
            path: "/v1/messages".to_owned(),
            query: None,
            body: json!({"model": model, "max_tokens": 8}),
            headers: Headers::default(),
        }
    }

    /// 束ねた後も、Wire は渡した [`AnthropicWire`] そのもの。
    ///
    /// 型が `Arc<AnthropicWire>` に固定されているので Bedrock 専用の Wire を
    /// 書くことはできず、実体としても同じものが preset に入っている。
    #[test]
    fn reuses_the_anthropic_wire_object() {
        let wire = wire();
        let preset = preset("bedrock", Arc::clone(&wire));

        assert!(
            std::ptr::eq(
                preset.wire() as *const dyn Wire as *const (),
                Arc::as_ptr(&wire) as *const (),
            ),
            "preset が持つ Wire が、渡したものと別の実体になっている"
        );
    }

    /// 公式と Bedrock は同じ Wire 実装を設定違いで使う。
    ///
    /// 差が出るのは接続先とモデル名だけで、ヘッダの整え方も直列化も共通。
    #[test]
    fn official_and_bedrock_share_one_wire_implementation() {
        let official_wire = Arc::new(AnthropicWire::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
            BTreeMap::new(),
        ));
        let official = anthropic::official("claude-personal", Arc::clone(&official_wire));
        let bedrock = preset("bedrock", wire());

        let via_official = official.wire().encode(request("claude-fable-5")).unwrap();
        let via_bedrock = bedrock.wire().encode(request("claude-fable-5")).unwrap();

        assert_eq!(
            via_official.url, "https://api.anthropic.com/v1/messages",
            "公式はクライアントの名前のまま"
        );
        assert_eq!(via_bedrock.url, format!("{BASE_URL}/v1/messages"));

        let sent = |request: &crate::egress::UpstreamRequest| -> Value {
            serde_json::from_slice(&request.body).unwrap()
        };
        assert_eq!(sent(&via_official)["model"], "claude-fable-5");
        assert_eq!(
            sent(&via_bedrock)["model"],
            "anthropic.claude-fable-5",
            "名前空間を付けないと 404 になる"
        );
        assert_eq!(
            via_official.headers.get("content-type"),
            via_bedrock.headers.get("content-type"),
            "方言が同じ部分は同じ結果になる"
        );
    }

    /// 差し替わるのは認証だけ。同じ credential でも載る先が違う。
    #[test]
    fn only_the_auth_differs_from_the_official_preset() {
        let credential = Credential::for_test("tok");
        let mut with_api_key = wire().encode(request("claude-opus-5")).unwrap();

        preset("bedrock", wire())
            .auth()
            .authorize(Some(&credential), &mut with_api_key)
            .unwrap();
        assert_eq!(with_api_key.headers.get("x-api-key"), Some("tok"));
        assert_eq!(
            with_api_key.headers.get("authorization"),
            None,
            "Bearer では 401 になる"
        );

        let mut with_bearer = wire().encode(request("claude-opus-5")).unwrap();
        OauthBearer::new("claude-personal")
            .apply(Some(&credential), &mut with_bearer.headers)
            .unwrap();
        assert_eq!(with_bearer.headers.get("authorization"), Some("Bearer tok"));
    }

    /// 枠照会 API を持たないことは型で示す (空実装を置かない)。
    #[test]
    fn has_no_quota_api() {
        assert!(preset("bedrock", wire()).quota_api().is_none());
    }

    /// 認証情報が要るのに渡されなければ、送る前に落とす。
    #[test]
    fn missing_credential_is_an_error() {
        let mut request = wire().encode(request("claude-opus-5")).unwrap();
        assert!(
            ApiKey::new("bedrock")
                .authorize(None, &mut request)
                .is_err()
        );
    }

    /// 既定の拒否リストと、設定での差し替え。
    #[test]
    fn beta_defaults_can_be_overridden() {
        let default = beta_policy(None);
        assert!(
            matches!(&default, beta::Policy::Deny(flags) if flags.contains("oauth-2025-04-20")),
            "既定は実測で分かっている分を落とす"
        );

        let overridden = beta_policy(Some(vec!["claude-code-20250219".to_owned()]));
        let beta::Policy::Deny(flags) = &overridden else {
            panic!("差し替えても落とす側のまま");
        };
        assert!(flags.contains("claude-code-20250219"));
        assert!(!flags.contains("oauth-2025-04-20"), "既定リストは使わない");
    }
}
