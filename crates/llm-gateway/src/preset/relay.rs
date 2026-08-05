//! 別の gateway へそのまま渡す preset。
//!
//! 転送先が自分の認証を持つので、こちらでは何も載せない。方言は Anthropic の
//! ままなので [`AnthropicWire`] をそのまま使い、接続先だけ差し替える。
//! 認証の軸だけが空いた形で、Bedrock と同じ composition の別の組み合わせ。

use std::sync::Arc;

use crate::Result;
use crate::credential::Credential;
use crate::egress::UpstreamRequest;
use crate::preset::anthropic::{AnthropicMetering, AnthropicWire, BetaFlags, beta};
use crate::provider::{Auth, Preset};
use crate::quota::Support;

/// 認証を載せない。
pub struct NoAuth;

impl Auth for NoAuth {
    fn authorize(
        &self,
        _credential: Option<&Credential>,
        _request: &mut UpstreamRequest,
    ) -> Result<()> {
        Ok(())
    }
}

/// 素通しの preset を組む。
///
/// 枠は転送先が持っているので、こちらから聞く口は無い。beta の取捨も転送先が
/// 決めるので、既定は素通し (学習した分だけは落とす)。
pub fn preset(name: &str, wire: Arc<AnthropicWire>) -> Preset {
    Preset::new(name, Arc::new(NoAuth), wire, Arc::new(AnthropicMetering))
        .with_negotiation(Arc::new(BetaFlags::new(beta::Policy::Passthrough)))
        // 枠を返すかどうかは転送先次第。こちらからは何とも言えない。
        .with_quota_support(Support::UpstreamDependent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{EgressRequest, Headers};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn wire() -> Arc<AnthropicWire> {
        Arc::new(AnthropicWire::new(
            "cpa",
            "http://127.0.0.1:8317",
            BTreeMap::new(),
        ))
    }

    /// 認証情報が無くても通る。ここが要求すると、転送先の認証と二重になる。
    #[test]
    fn adds_no_auth_of_its_own() {
        let preset = preset("cpa", wire());
        let mut request = preset
            .wire()
            .encode(EgressRequest {
                path: "/v1/messages".to_owned(),
                query: None,
                body: json!({"model": "gpt-5.6-sol"}),
                headers: Headers::default(),
            })
            .unwrap();

        preset.auth().authorize(None, &mut request).unwrap();

        assert_eq!(request.headers.get("authorization"), None);
        assert_eq!(request.headers.get("x-api-key"), None);
        assert!(preset.quota_api().is_none(), "枠は転送先が持っている");
        assert_eq!(preset.quota_support(), Support::UpstreamDependent);
    }

    /// beta は転送先が判断するので触らない。
    #[test]
    fn leaves_the_beta_flags_to_the_relay_target() {
        const CLIENT_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
        let mut headers = Headers::new(vec![("anthropic-beta".to_owned(), CLIENT_BETA.to_owned())]);

        preset("cpa", wire())
            .negotiation()
            .expect("学習した分は落とせる")
            .prepare(&mut headers, &[]);

        assert_eq!(headers.get("anthropic-beta"), Some(CLIENT_BETA));
    }
}
