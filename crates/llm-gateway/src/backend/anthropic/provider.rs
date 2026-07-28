//! Messages API を話す upstream の実装。
//!
//! 3 つとも同じ API を話すので、転送とストリーム中継は共通のものを使う。
//! ここにあるのは接続先・認証方式・リクエストの微調整だけ。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::credential::Credential;
use crate::{Error, Result};

use super::{Headers, Provider, beta, rewrite_model};

/// Anthropic 公式。サブスクの OAuth token をそのまま載せる。
///
/// リクエストは素通しでよい。偽装も beta の加工も要らないことを実測で
/// 確認している (system prompt 無しでも 200、クライアントが送る beta も全て通る)。
pub struct Official {
    name: String,
    base_url: String,
    extra_headers: BTreeMap<String, String>,
}

impl Official {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            extra_headers,
        }
    }
}

impl Provider for Official {
    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn authorize(&self, headers: &mut Headers, credential: Option<&Credential>) -> Result<()> {
        let c = credential.ok_or_else(|| Error::Credential {
            id: self.name.clone(),
            reason: "認証情報が渡されていません".to_owned(),
        })?;
        headers.set("authorization", c.bearer());
        Ok(())
    }

    fn adapt(&self, _body: &mut Value, headers: &mut Headers) {
        headers.extend_from(&self.extra_headers);
    }
}

/// Bedrock の Anthropic 互換。
///
/// 公式との違いは 3 つ: `x-api-key` で認証する、モデル名が自分の名前空間、
/// 受け付けない beta フラグがある。
pub struct Bedrock {
    name: String,
    base_url: String,
    extra_headers: BTreeMap<String, String>,
    beta_policy: beta::Policy,
    /// クライアントの名前 → upstream の名前。
    model_map: BTreeMap<String, String>,
}

impl Bedrock {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
        deny_beta: Option<Vec<String>>,
        model_map: BTreeMap<String, String>,
    ) -> Self {
        let beta_policy = match deny_beta {
            Some(flags) => beta::Policy::Deny(flags.into_iter().collect()),
            None => beta::Policy::bedrock(),
        };
        Self {
            name: name.into(),
            base_url: base_url.into(),
            extra_headers,
            beta_policy,
            model_map,
        }
    }
}

impl Provider for Bedrock {
    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn authorize(&self, headers: &mut Headers, credential: Option<&Credential>) -> Result<()> {
        let c = credential.ok_or_else(|| Error::Credential {
            id: self.name.clone(),
            reason: "認証情報が渡されていません".to_owned(),
        })?;
        headers.set("x-api-key", c.api_key());
        Ok(())
    }

    fn beta_policy(&self) -> beta::Policy {
        self.beta_policy.clone()
    }

    fn adapt(&self, body: &mut Value, headers: &mut Headers) {
        if let Some(model) = body.get("model").and_then(Value::as_str)
            && let Some(upstream) = self.model_map.get(model)
        {
            let upstream = upstream.clone();
            rewrite_model(body, &upstream);
        }

        headers.extend_from(&self.extra_headers);
    }
}

/// 別の gateway へそのまま渡す。
///
/// 転送先が認証を持つので、こちらでは何も載せない。移行期に gpt 系を
/// cpa へ流すために使う。
pub struct Relay {
    name: String,
    base_url: String,
    extra_headers: BTreeMap<String, String>,
}

impl Relay {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            extra_headers,
        }
    }
}

impl Provider for Relay {
    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn needs_credential(&self) -> bool {
        false
    }

    fn authorize(&self, _headers: &mut Headers, _credential: Option<&Credential>) -> Result<()> {
        Ok(())
    }

    fn adapt(&self, _body: &mut Value, headers: &mut Headers) {
        headers.extend_from(&self.extra_headers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Claude Code 2.1.220 が実際に送る beta の束。
    const CLIENT_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
thinking-token-count-2026-05-13,context-management-2025-06-27,\
prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
extended-cache-ttl-2025-04-11";

    fn client_headers() -> Headers {
        Headers::new(vec![
            ("anthropic-version".into(), "2023-06-01".into()),
            ("anthropic-beta".into(), CLIENT_BETA.into()),
        ])
    }

    /// 転送側と同じ手順で beta を整える (適用は provider の外)。
    fn negotiate_beta(provider: &dyn Provider, headers: &mut Headers) -> Vec<String> {
        provider.beta_policy().apply_to(headers)
    }

    fn official() -> Official {
        Official::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
        )
    }

    fn bedrock() -> Bedrock {
        Bedrock::new(
            "bedrock",
            "https://bedrock-mantle.us-east-1.api.aws/anthropic",
            BTreeMap::new(),
            None,
            BTreeMap::from([(
                "claude-fable-5".to_owned(),
                "anthropic.claude-fable-5".to_owned(),
            )]),
        )
    }

    /// 公式はリクエストを触らない。beta も全部そのまま通す。
    #[test]
    fn official_passes_request_through() {
        let mut body = json!({"model": "claude-opus-5", "max_tokens": 8});
        let mut headers = client_headers();
        let before = body.clone();

        official().adapt(&mut body, &mut headers);
        negotiate_beta(&official(), &mut headers);

        assert_eq!(body, before, "ボディを触らない");
        assert_eq!(
            headers.get("anthropic-beta"),
            Some(CLIENT_BETA),
            "beta も落とさない"
        );
    }

    /// Bedrock はモデル名を自分の名前空間に替える。替えないと 404 になる。
    #[test]
    fn bedrock_rewrites_model_name() {
        let mut body = json!({"model": "claude-fable-5", "max_tokens": 8});
        let mut headers = client_headers();

        bedrock().adapt(&mut body, &mut headers);

        assert_eq!(body["model"], "anthropic.claude-fable-5");
        assert_eq!(body["max_tokens"], 8, "他は触らない");
    }

    /// 対応表に無いモデルはそのまま送る (勝手に書き換えない)。
    #[test]
    fn bedrock_leaves_unmapped_model_alone() {
        let mut body = json!({"model": "claude-opus-5"});
        bedrock().adapt(&mut body, &mut client_headers());
        assert_eq!(body["model"], "claude-opus-5");
    }

    /// Bedrock が拒否する 4 つだけ落ち、受け付ける 4 つは残る。
    #[test]
    fn bedrock_drops_only_rejected_beta_flags() {
        let mut headers = client_headers();
        negotiate_beta(&bedrock(), &mut headers);

        let kept = headers.get("anthropic-beta").expect("残るものがある");
        for gone in [
            "oauth-2025-04-20",
            "prompt-caching-scope-2026-01-05",
            "advisor-tool-2026-03-01",
            "extended-cache-ttl-2025-04-11",
        ] {
            assert!(!kept.contains(gone), "{gone} は落とす: {kept}");
        }
        for stays in [
            "interleaved-thinking-2025-05-14",
            "thinking-token-count-2026-05-13",
            "context-management-2025-06-27",
            "claude-code-20250219",
        ] {
            assert!(kept.contains(stays), "{stays} は残す: {kept}");
        }
    }

    /// 全部拒否されるならヘッダごと消す (空の値を送らない)。
    #[test]
    fn bedrock_removes_header_when_nothing_survives() {
        let mut headers = Headers::new(vec![(
            "anthropic-beta".into(),
            "oauth-2025-04-20,advisor-tool-2026-03-01".into(),
        )]);
        negotiate_beta(&bedrock(), &mut headers);
        assert_eq!(headers.get("anthropic-beta"), None);
    }

    /// 落とすリストは設定で差し替えられる。
    #[test]
    fn bedrock_deny_list_can_be_overridden() {
        let provider = Bedrock::new(
            "b",
            "https://example.invalid",
            BTreeMap::new(),
            Some(vec!["claude-code-20250219".to_owned()]),
            BTreeMap::new(),
        );
        let mut headers = client_headers();
        negotiate_beta(&provider, &mut headers);

        let kept = headers.get("anthropic-beta").unwrap();
        assert!(!kept.contains("claude-code-20250219"), "指定した分は落ちる");
        assert!(kept.contains("oauth-2025-04-20"), "既定リストは使わない");
    }

    #[test]
    fn auth_header_differs_by_provider() {
        let credential = crate::credential::Credential::for_test("tok");

        let mut h = Headers::default();
        official().authorize(&mut h, Some(&credential)).unwrap();
        assert_eq!(h.get("authorization"), Some("Bearer tok"));
        assert_eq!(h.get("x-api-key"), None);

        let mut h = Headers::default();
        bedrock().authorize(&mut h, Some(&credential)).unwrap();
        assert_eq!(h.get("x-api-key"), Some("tok"));
        assert_eq!(h.get("authorization"), None, "Bearer では 401 になる");
    }

    /// 認証情報が要るのに渡されなければ、送る前に落とす。
    #[test]
    fn missing_credential_is_an_error() {
        assert!(official().authorize(&mut Headers::default(), None).is_err());
        assert!(bedrock().authorize(&mut Headers::default(), None).is_err());
    }

    /// relay は認証を付けない (転送先が持っている)。
    #[test]
    fn relay_adds_no_auth() {
        let relay = Relay::new("cpa", "http://127.0.0.1:8317", BTreeMap::new());
        assert!(!relay.needs_credential());

        let mut h = client_headers();
        relay.authorize(&mut h, None).unwrap();
        relay.adapt(&mut json!({"model": "gpt-5.6-sol"}), &mut h);
        negotiate_beta(&relay, &mut h);

        assert_eq!(h.get("authorization"), None);
        assert_eq!(h.get("x-api-key"), None);
        assert_eq!(
            h.get("anthropic-beta"),
            Some(CLIENT_BETA),
            "転送先が判断するので触らない"
        );
    }

    /// 設定で足したヘッダが載る。
    #[test]
    fn extra_headers_are_applied() {
        let provider = Official::new(
            "c",
            "https://api.anthropic.com",
            BTreeMap::from([("x-trace".to_owned(), "on".to_owned())]),
        );
        let mut h = Headers::default();
        provider.adapt(&mut json!({}), &mut h);
        assert_eq!(h.get("x-trace"), Some("on"));
    }
}
