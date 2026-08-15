//! Bedrock の Anthropic 互換 — 認証だけ独自の composite preset (DR-0014 §6)。
//!
//! 公式との違いは 3 つしかない: `x-api-key` で認証する、モデル名が自分の
//! 名前空間、受け付けない beta フラグがある。方言は同じなので
//! [`AnthropicWire`] を**書き直さず**、接続先を設定として渡すだけで再利用する
//! (モデル名の読み替えは discovery が答えるので、方言の実装には無い)。
//!
//! 認証の軸と方言の軸が直交している (DR-0004 の 2 軸分離) ことの実証であり、
//! 小 trait 分割が正しく切れているかの試金石でもある — ここで Wire を写経する
//! ことになったら、切り方が間違っている。

use std::sync::Arc;

use crate::credential::Credential;
use crate::egress::{Headers, UpstreamRequest};
use crate::preset::anthropic::{AnthropicMetering, AnthropicWire, BetaFlags, beta};
use crate::provider::{Auth, Preset};
use crate::quota::Support;
use crate::{Error, Result};

/// Bedrock の認証。API キーを `x-api-key` に載せる。
///
/// [`OauthBearer`](crate::preset::anthropic::OauthBearer) と載せる先が違うだけに
/// 見えるが、`authorization` で送ると 401 になる (実測)。
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
            reason: "no credential was provided".to_owned(),
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
/// `wire` には Bedrock の接続先を入れたものを渡す。枠照会 API は無いので
/// 足さない — 呼んでから「未対応」を返す実装を置かない。
///
/// `deny_beta` を渡すと既定の一覧は使わない (実測で分かっている分より、運用者が
/// 今見ている upstream の方が新しい)。
pub fn preset(name: &str, wire: Arc<AnthropicWire>, deny_beta: Option<Vec<String>>) -> Preset {
    let policy = match deny_beta {
        Some(flags) => beta::Policy::Deny(flags.into_iter().collect()),
        None => beta::Policy::bedrock(),
    };
    Preset::new(
        name,
        Arc::new(ApiKey::new(name)),
        wire,
        Arc::new(AnthropicMetering),
    )
    .with_negotiation(Arc::new(BetaFlags::new(policy)))
    // 使用量は別の IAM アクションで、実行権限しかない API キーでは取れない。
    .with_quota_support(Support::NotApplicable)
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

    /// Claude Code 2.1.220 が実際に送る beta の束。
    const CLIENT_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
thinking-token-count-2026-05-13,context-management-2025-06-27,\
prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
extended-cache-ttl-2025-04-11";

    fn wire() -> Arc<AnthropicWire> {
        Arc::new(AnthropicWire::new("bedrock", BASE_URL, BTreeMap::new()))
    }

    fn request(model: &str) -> EgressRequest {
        EgressRequest {
            path: "/v1/messages".to_owned(),
            query: None,
            body: json!({"model": model, "max_tokens": 8}),
            headers: Headers::default(),
        }
    }

    fn bedrock() -> Preset {
        preset("bedrock", wire(), None)
    }

    /// 束ねた後も、Wire は渡した [`AnthropicWire`] そのもの。
    ///
    /// 型が `Arc<AnthropicWire>` に固定されているので Bedrock 専用の Wire を
    /// 書くことはできず、実体としても同じものが preset に入っている。
    #[test]
    fn reuses_the_anthropic_wire_object() {
        let wire = wire();
        let preset = preset("bedrock", Arc::clone(&wire), None);

        assert!(
            std::ptr::eq(
                preset.wire() as *const dyn Wire as *const (),
                Arc::as_ptr(&wire) as *const (),
            ),
            "the Wire held by the preset is a different instance from the one passed in"
        );
    }

    /// 公式と Bedrock は同じ Wire 実装を設定違いで使う。
    ///
    /// 差が出るのは接続先だけで、ヘッダの整え方も直列化も共通。
    #[test]
    fn official_and_bedrock_share_one_wire_implementation() {
        let official_wire = Arc::new(AnthropicWire::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
        ));
        let official = anthropic::official("claude-personal", official_wire);

        let via_official = official.wire().encode(request("claude-fable-5")).unwrap();
        let via_bedrock = bedrock().wire().encode(request("claude-fable-5")).unwrap();

        assert_eq!(
            via_official.upstream.url, "https://api.anthropic.com/v1/messages",
            "official keeps the client's name as-is"
        );
        assert_eq!(via_bedrock.upstream.url, format!("{BASE_URL}/v1/messages"));

        let sent =
            |request: &UpstreamRequest| -> Value { serde_json::from_slice(&request.body).unwrap() };
        assert_eq!(
            sent(&via_official.upstream)["model"],
            sent(&via_bedrock.upstream)["model"],
            "same dialect, so the same body is sent (name remapping happens outside Wire)"
        );
        assert_eq!(
            via_official.upstream.headers.get("content-type"),
            via_bedrock.upstream.headers.get("content-type")
        );
    }

    /// 差し替わるのは認証だけ。同じ credential でも載る先が違う。
    #[test]
    fn only_the_auth_differs_from_the_official_preset() {
        let credential = Credential::for_test("tok");
        let mut with_api_key = wire().encode(request("claude-opus-5")).unwrap();

        bedrock()
            .auth()
            .authorize(Some(&credential), &mut with_api_key.upstream)
            .unwrap();
        assert_eq!(with_api_key.upstream.headers.get("x-api-key"), Some("tok"));
        assert_eq!(
            with_api_key.upstream.headers.get("authorization"),
            None,
            "Bearer results in a 401"
        );

        let mut with_bearer = wire().encode(request("claude-opus-5")).unwrap();
        OauthBearer::new("claude-personal")
            .apply(Some(&credential), &mut with_bearer.upstream.headers)
            .unwrap();
        assert_eq!(
            with_bearer.upstream.headers.get("authorization"),
            Some("Bearer tok")
        );
    }

    /// 枠照会 API を持たないことは型で示す (空実装を置かない)。
    #[test]
    fn has_no_quota_api() {
        assert!(bedrock().quota_api().is_none());
        assert_eq!(
            bedrock().quota_support(),
            Support::NotApplicable,
            "not that there's no endpoint to ask, it's simply unobtainable"
        );
    }

    /// 認証情報が要るのに渡されなければ、送る前に落とす。
    #[test]
    fn missing_credential_is_an_error() {
        let mut request = wire().encode(request("claude-opus-5")).unwrap();
        assert!(
            ApiKey::new("bedrock")
                .authorize(None, &mut request.upstream)
                .is_err()
        );
    }

    /// Bedrock が拒否する 4 つだけ落ち、受け付ける 4 つは残る。
    #[test]
    fn drops_only_rejected_beta_flags() {
        let mut headers = Headers::new(vec![("anthropic-beta".to_owned(), CLIENT_BETA.to_owned())]);
        bedrock()
            .negotiation()
            .expect("has beta negotiation")
            .prepare(&mut headers, &[]);

        let kept = headers.get("anthropic-beta").expect("something remains");
        for gone in [
            "oauth-2025-04-20",
            "prompt-caching-scope-2026-01-05",
            "advisor-tool-2026-03-01",
            "extended-cache-ttl-2025-04-11",
        ] {
            assert!(!kept.contains(gone), "{gone} should be dropped: {kept}");
        }
        for stays in [
            "interleaved-thinking-2025-05-14",
            "thinking-token-count-2026-05-13",
            "context-management-2025-06-27",
            "claude-code-20250219",
        ] {
            assert!(kept.contains(stays), "{stays} should be kept: {kept}");
        }
    }

    /// 落とすリストは設定で差し替えられる。差し替えたら既定は使わない。
    #[test]
    fn the_deny_list_can_be_overridden() {
        let overridden = preset("b", wire(), Some(vec!["claude-code-20250219".to_owned()]));
        let mut headers = Headers::new(vec![("anthropic-beta".to_owned(), CLIENT_BETA.to_owned())]);
        overridden
            .negotiation()
            .expect("has beta negotiation")
            .prepare(&mut headers, &[]);

        let kept = headers.get("anthropic-beta").unwrap();
        assert!(!kept.contains("claude-code-20250219"), "the specified entry is dropped");
        assert!(kept.contains("oauth-2025-04-20"), "the default list is not used");
    }
}
