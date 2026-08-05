//! Anthropic Messages API を話す upstream の方言一式。
//!
//! この方言を話すのは公式だけではない。Bedrock ([`crate::preset::bedrock`]) は
//! 認証だけ差し替えて同じ [`AnthropicWire`] を使い、別 gateway への素通し
//! ([`crate::preset::relay`]) は認証を載せずに同じ Wire を使う。方言を持つ側が
//! 1 つなので、公式に効く直しはそのまま両方へ届く。
//!
//! 内部正規形が Anthropic Messages 形式 (DR-0014 §5) なので、この Wire の
//! 変換は「接続先・モデル名・付随ヘッダを合わせる」だけで済む。

mod auth;
pub mod beta;
mod metering;
mod quota_api;
mod wire;

use std::sync::Arc;

use serde_json::Value;

pub use auth::OauthBearer;
pub use metering::AnthropicMetering;
pub use quota_api::OauthUsage;
pub use wire::AnthropicWire;

use crate::provider::Preset;
use crate::{Error, Result};

/// Anthropic 公式の preset。
///
/// サブスクの OAuth token をそのまま載せ、トークンを消費しない枠照会 API を
/// 持つ (DR-0007)。
pub fn official(name: &str, wire: Arc<AnthropicWire>) -> Preset {
    let quota_api = OauthUsage::new(wire.base_url());
    Preset::new(
        name,
        Arc::new(OauthBearer::new(name)),
        wire,
        Arc::new(AnthropicMetering),
        Some(Arc::new(quota_api)),
    )
}

/// ボディの `model` を upstream が求める名前に替える。
///
/// Bedrock は `anthropic.claude-fable-5` を要求し、クライアントが送る
/// `claude-fable-5` では 404 になる。
pub fn rewrite_model(body: &mut Value, upstream_name: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_owned(), Value::String(upstream_name.to_owned()));
    }
}

/// ボディからモデル名を読む。
pub fn model_of(body: &Value) -> Result<&str> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Config("リクエストに model がありません".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn wire() -> Arc<AnthropicWire> {
        Arc::new(AnthropicWire::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
            BTreeMap::new(),
        ))
    }

    /// 公式は枠照会 API を持つ。持たない preset との差は型に出る (DR-0014 §2)。
    #[test]
    fn official_has_a_quota_api() {
        let preset = official("claude-personal", wire());

        assert_eq!(preset.name(), "claude-personal");
        assert!(preset.quota_api().is_some());
    }

    #[test]
    fn model_is_rewritten_in_place() {
        let mut body = json!({"model": "claude-fable-5", "max_tokens": 8});
        rewrite_model(&mut body, "anthropic.claude-fable-5");

        assert_eq!(body["model"], "anthropic.claude-fable-5");
        assert_eq!(body["max_tokens"], 8, "他の項目は触らない");
    }

    #[test]
    fn reads_model_name() {
        assert_eq!(
            model_of(&json!({"model": "claude-opus-5"})).unwrap(),
            "claude-opus-5"
        );
        assert!(model_of(&json!({})).is_err());
        assert!(model_of(&json!({"model": ""})).is_err());
        assert!(model_of(&json!({"model": 42})).is_err());
    }
}
