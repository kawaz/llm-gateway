//! provider preset の実装置き場 (DR-0014)。
//!
//! core は [`crate::provider`] で小 trait と入出力の契約だけを規定し、どんな
//! provider が居るかを知らない。ここから下が「その IF の provider ごとの
//! impl」で、preset は Auth / Wire / Metering / 任意の capability を束ねたもの。
//!
//! 束ね方は 3 通りある。方言も認証も自前で持つもの ([`anthropic`])、認証だけ
//! 差し替えて方言を借りるもの ([`bedrock`])、認証を持たず方言だけ借りるもの
//! ([`relay`])。認証の軸と方言の軸が直交しているので、後の 2 つは Wire を
//! 書き直さずに設定だけで作れる。
//!
//! モデルの単価表 ([`pricing`]) もここに置く。いくら掛かるかは upstream が
//! 決める事実で、集計の器が持つものではない (DR-0014 §4)。
//!
//! **設定の `type` がどの束ね方を指すかを知るのは [`from_spec`] だけ**。
//! router はここが返した preset を名前で引くだけで、provider の顔ぶれを
//! 知らずに済む (DR-0014 §3 の判定基準)。

pub mod anthropic;
pub mod bedrock;
pub mod pricing;
pub mod relay;

use std::sync::Arc;

use anthropic::AnthropicWire;

use crate::config::CredentialSpec;
use crate::provider::Preset;

/// 設定 1 件を preset へ組み上げる。
pub fn from_spec(name: &str, spec: &CredentialSpec) -> Preset {
    let wire = Arc::new(AnthropicWire::new(name, spec.url(), spec.headers().clone()));
    match spec {
        CredentialSpec::ClaudeOauth { .. } => anthropic::official(name, wire),
        CredentialSpec::ClaudeBedrock { deny_beta, .. } => {
            bedrock::preset(name, wire, deny_beta.clone())
        }
        // Responses API への変換は未実装。それまでは素通しで凌ぐ。
        CredentialSpec::CodexOauth { .. } | CredentialSpec::Relay { .. } => {
            relay::preset(name, wire)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(toml: &str) -> CredentialSpec {
        toml::from_str(toml).expect("設定として読める")
    }

    fn oauth() -> CredentialSpec {
        spec(r#"type = "claude_oauth""#)
    }

    fn bedrock() -> CredentialSpec {
        spec(
            r#"type = "claude_bedrock"
url = "https://bedrock.invalid/anthropic""#,
        )
    }

    fn relay() -> CredentialSpec {
        spec(
            r#"type = "relay"
url = "http://127.0.0.1:8317""#,
        )
    }

    /// 束ね方の違いは capability の有無に出る。
    #[test]
    fn the_config_type_decides_which_capabilities_exist() {
        assert!(
            from_spec("claude-personal", &oauth()).quota_api().is_some(),
            "枠を聞ける口を持つ"
        );
        assert!(
            from_spec("bedrock", &bedrock()).quota_api().is_none(),
            "Bedrock に枠照会 API は無い"
        );
        assert!(
            from_spec("cpa", &relay()).quota_api().is_none(),
            "枠は転送先が持っている"
        );
    }

    /// 束ね方ごとに、枠について何が言えるかも決まる。
    ///
    /// 「取れるがまだ観測していない」「仕組みとして取れない」「転送先次第」は
    /// upstream の事情なので、閲覧の口ではなく preset が宣言する (DR-0007)。
    #[test]
    fn the_config_type_decides_what_can_be_said_about_quota() {
        use crate::quota::Support;

        assert_eq!(
            from_spec("claude-personal", &oauth()).quota_support(),
            Support::Unobserved,
            "応答ヘッダに載るので、使えば見える"
        );
        assert_eq!(
            from_spec("bedrock", &bedrock()).quota_support(),
            Support::NotApplicable,
            "使用量は別の権限で、実行権限しかない鍵では取れない"
        );
        assert_eq!(
            from_spec("cpa", &relay()).quota_support(),
            Support::UpstreamDependent,
            "転送先が返さないものは見えない"
        );
    }

    /// 名前と接続先は設定から届く。
    #[test]
    fn the_preset_carries_the_configured_name_and_url() {
        let spec = spec(
            r#"type = "claude_oauth"
url = "https://proxy.invalid""#,
        );
        let preset = from_spec("claude-work", &spec);

        assert_eq!(preset.name(), "claude-work");
        let encoded = preset
            .wire()
            .encode(crate::egress::EgressRequest {
                path: "/v1/messages".to_owned(),
                query: None,
                body: serde_json::json!({"model": "m"}),
                headers: crate::egress::Headers::default(),
            })
            .unwrap();
        assert_eq!(encoded.url, "https://proxy.invalid/v1/messages");
    }

    /// 設定で足したヘッダは、どの束ね方でも同じ Wire が載せる。
    #[test]
    fn extra_headers_reach_the_wire() {
        let spec = spec(
            r#"type = "relay"
url = "http://127.0.0.1:8317"
headers = { x-trace = "on" }"#,
        );
        let encoded = from_spec("cpa", &spec)
            .wire()
            .encode(crate::egress::EgressRequest {
                path: "/v1/messages".to_owned(),
                query: None,
                body: serde_json::json!({"model": "m"}),
                headers: crate::egress::Headers::default(),
            })
            .unwrap();

        assert_eq!(encoded.headers.get("x-trace"), Some("on"));
        assert_eq!(
            spec.headers(),
            &BTreeMap::from([("x-trace".to_owned(), "on".to_owned())])
        );
    }
}
