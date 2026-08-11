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
pub mod openai;
pub mod pricing;
pub mod relay;

use std::sync::Arc;

use anthropic::AnthropicWire;

use crate::config::{Config, CredentialSpec, Provider, RouteSpec};
use crate::provider::Preset;

/// route と認証情報を provider preset へ組み上げる。
pub fn from_spec(name: &str, route: &RouteSpec, config: &Config) -> Preset {
    match (route.provider, route.credential(config)) {
        (Provider::Anthropic, credential) => {
            let wire = Arc::new(AnthropicWire::new(
                name,
                route.url(),
                route.headers().clone(),
            ));
            match credential {
                Some(CredentialSpec::ClaudeOauth) => anthropic::official(name, wire),
                Some(CredentialSpec::BedrockApiKey) => {
                    bedrock::preset(name, wire, route.deny_beta.clone())
                }
                None => relay::preset(name, wire),
                _ => unreachable!("config validation guarantees a supported route composition"),
            }
        }
        (Provider::Openai, Some(CredentialSpec::CodexOauth)) => {
            let wire = Arc::new(openai::OpenAiWire::new(
                name,
                route.url(),
                route.headers().clone(),
            ));
            openai::chatgpt(name, wire)
        }
        _ => unreachable!("config validation guarantees a supported route composition"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
[credentials.claude]
type = "claude_oauth"
[credentials.bedrock]
type = "bedrock_api_key"
[routes.claude]
provider = "anthropic"
credential = "claude"
url = "https://proxy.invalid"
[routes.bedrock]
provider = "anthropic"
credential = "bedrock"
url = "https://bedrock.invalid/anthropic"
[routes.cpa]
provider = "anthropic"
url = "http://127.0.0.1:8317"
headers = { x-trace = "on" }
"#,
        )
        .expect("設定として読める")
    }

    fn preset(config: &Config, name: &str) -> Preset {
        from_spec(name, &config.routes[name], config)
    }

    /// 認証情報と方言の組み合わせで capability が決まる。
    #[test]
    fn route_composition_decides_capabilities() {
        let config = config();
        assert!(preset(&config, "claude").quota_api().is_some());
        assert!(preset(&config, "bedrock").quota_api().is_none());
        assert!(preset(&config, "cpa").quota_api().is_none());
    }

    /// route の URL と追加ヘッダは、認証情報とは独立して Wire へ届く。
    #[test]
    fn route_settings_reach_the_wire() {
        let config = config();
        let encoded = preset(&config, "cpa")
            .wire()
            .encode(crate::egress::EgressRequest {
                path: "/v1/messages".to_owned(),
                query: None,
                body: serde_json::json!({"model": "m"}),
                headers: crate::egress::Headers::default(),
            })
            .unwrap();

        assert_eq!(encoded.upstream.url, "http://127.0.0.1:8317/v1/messages");
        assert_eq!(encoded.upstream.headers.get("x-trace"), Some("on"));
    }
}
