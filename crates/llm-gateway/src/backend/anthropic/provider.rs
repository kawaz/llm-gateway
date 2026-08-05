//! 旧 `Provider` の呼び出しを preset へ橋渡しする (facade)。
//!
//! 3 つとも中身は [`Adapter`] 1 つで、違うのは**どう束ねたか**だけ。組み立ての
//! 判断は [`crate::preset`] 側にあり、ここは名前と引数の形を保つだけ。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::Result;
use crate::credential::Credential;
use crate::preset::{anthropic, bedrock, relay};
use crate::provider::Preset;

use super::{Headers, Provider, beta};

/// preset を旧 `Provider` の呼び口に合わせる。
///
/// `wire` は preset が持つものと同じ実体。ヘッダだけを触る旧 API
/// ([`Provider::adapt`] / [`Provider::authorize`]) は、直列化前の形で
/// 呼ばれるのでここから直接届ける。
pub struct Adapter {
    preset: Preset,
    wire: Arc<anthropic::AnthropicWire>,
    beta_policy: beta::Policy,
}

impl Adapter {
    fn name(&self) -> &str {
        self.preset.name()
    }

    fn base_url(&self) -> &str {
        self.wire.base_url()
    }

    fn authorize(&self, headers: &mut Headers, credential: Option<&Credential>) -> Result<()> {
        // Auth は直列化済みのリクエストに署名する。ここではまだボディが無いので、
        // ヘッダだけを預けて戻す。
        let mut request = crate::egress::UpstreamRequest {
            url: String::new(),
            headers: std::mem::take(headers),
            body: bytes::Bytes::new(),
        };
        let applied = self.preset.auth().authorize(credential, &mut request);
        *headers = request.headers;
        applied
    }

    fn adapt(&self, body: &mut Value, headers: &mut Headers) {
        self.wire.adapt(body, headers);
    }
}

/// 3 つの名前は束ね方の違いでしかないので、呼び口の実装は 1 つを配る。
macro_rules! facade {
    ($name:ident) => {
        impl Provider for $name {
            fn name(&self) -> &str {
                self.0.name()
            }

            fn base_url(&self) -> &str {
                self.0.base_url()
            }

            fn authorize(
                &self,
                headers: &mut Headers,
                credential: Option<&Credential>,
            ) -> Result<()> {
                self.0.authorize(headers, credential)
            }

            fn beta_policy(&self) -> beta::Policy {
                self.0.beta_policy.clone()
            }

            fn adapt(&self, body: &mut Value, headers: &mut Headers) {
                self.0.adapt(body, headers)
            }

            fn preset(&self) -> &Preset {
                &self.0.preset
            }
        }
    };
}

/// Anthropic 公式。
pub struct Official(Adapter);

impl Official {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Self {
        let name = name.into();
        let wire = Arc::new(anthropic::AnthropicWire::new(
            &name,
            base_url,
            extra_headers,
            BTreeMap::new(),
        ));
        Self(Adapter {
            preset: anthropic::official(&name, Arc::clone(&wire)),
            wire,
            beta_policy: beta::Policy::Passthrough,
        })
    }
}

facade!(Official);

/// Bedrock の Anthropic 互換。
pub struct Bedrock(Adapter);

impl Bedrock {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
        deny_beta: Option<Vec<String>>,
        model_map: BTreeMap<String, String>,
    ) -> Self {
        let name = name.into();
        let wire = Arc::new(anthropic::AnthropicWire::new(
            &name,
            base_url,
            extra_headers,
            model_map,
        ));
        Self(Adapter {
            preset: bedrock::preset(&name, Arc::clone(&wire)),
            wire,
            beta_policy: bedrock::beta_policy(deny_beta),
        })
    }
}

facade!(Bedrock);

/// 別の gateway へそのまま渡す。
pub struct Relay(Adapter);

impl Relay {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        extra_headers: BTreeMap<String, String>,
    ) -> Self {
        let name = name.into();
        let wire = Arc::new(anthropic::AnthropicWire::new(
            &name,
            base_url,
            extra_headers,
            BTreeMap::new(),
        ));
        Self(Adapter {
            preset: relay::preset(&name, Arc::clone(&wire)),
            wire,
            beta_policy: beta::Policy::Passthrough,
        })
    }
}

facade!(Relay);

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
        bedrock().adapt(&mut body, &mut client_headers());

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
        let credential = Credential::for_test("tok");

        let mut headers = Headers::default();
        official()
            .authorize(&mut headers, Some(&credential))
            .unwrap();
        assert_eq!(headers.get("authorization"), Some("Bearer tok"));
        assert_eq!(headers.get("x-api-key"), None);

        let mut headers = Headers::default();
        bedrock()
            .authorize(&mut headers, Some(&credential))
            .unwrap();
        assert_eq!(headers.get("x-api-key"), Some("tok"));
        assert_eq!(headers.get("authorization"), None, "Bearer では 401 になる");
    }

    /// ヘッダだけを預ける橋渡しでも、既に載っている分は失わない。
    #[test]
    fn authorize_keeps_the_headers_it_was_given() {
        let credential = Credential::for_test("tok");
        let mut headers = client_headers();

        official()
            .authorize(&mut headers, Some(&credential))
            .unwrap();

        assert_eq!(headers.get("anthropic-version"), Some("2023-06-01"));
        assert_eq!(headers.get("anthropic-beta"), Some(CLIENT_BETA));
        assert_eq!(headers.get("authorization"), Some("Bearer tok"));
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

        let mut headers = client_headers();
        relay.authorize(&mut headers, None).unwrap();
        relay.adapt(&mut json!({"model": "gpt-5.6-sol"}), &mut headers);
        negotiate_beta(&relay, &mut headers);

        assert_eq!(headers.get("authorization"), None);
        assert_eq!(headers.get("x-api-key"), None);
        assert_eq!(
            headers.get("anthropic-beta"),
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
        let mut headers = Headers::default();
        provider.adapt(&mut json!({}), &mut headers);
        assert_eq!(headers.get("x-trace"), Some("on"));
    }

    /// 名前と接続先は preset / wire から引く (facade は覚え直さない)。
    #[test]
    fn name_and_base_url_come_from_the_preset() {
        assert_eq!(official().name(), "claude-personal");
        assert_eq!(official().base_url(), "https://api.anthropic.com");
        assert_eq!(official().preset().name(), "claude-personal");
    }

    /// 束ね方の違いは capability の有無に出る。
    #[test]
    fn only_the_official_preset_can_ask_for_quota() {
        assert!(official().preset().quota_api().is_some());
        assert!(bedrock().preset().quota_api().is_none());
        assert!(
            Relay::new("cpa", "http://127.0.0.1:8317", BTreeMap::new())
                .preset()
                .quota_api()
                .is_none()
        );
    }
}
