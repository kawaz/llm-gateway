//! Anthropic Messages API を話す upstream の方言一式。
//!
//! この方言を話すのは公式だけではない。Bedrock ([`crate::preset::bedrock`]) は
//! 認証だけ差し替えて同じ [`AnthropicWire`] を使い、別 gateway への素通し
//! ([`crate::preset::relay`]) は認証を載せずに同じ Wire を使う。方言を持つ側が
//! 1 つなので、公式に効く直しはそのまま両方へ届く。
//!
//! 内部正規形が Anthropic Messages 形式 (DR-0014 §5) なので、この Wire の
//! 変換は「接続先と付随ヘッダを合わせる」だけで済む。

mod auth;
pub mod beta;
mod metering;
mod negotiation;
mod quota_api;
mod wire;

use std::sync::Arc;

pub use auth::OauthBearer;
pub use metering::AnthropicMetering;
pub use negotiation::BetaFlags;
pub use quota_api::OauthUsage;
pub use wire::AnthropicWire;

use crate::provider::Preset;
use crate::quota::Support;

/// Anthropic 公式の preset。
///
/// サブスクの OAuth token をそのまま載せ、トークンを消費しない枠照会 API を
/// 持つ (DR-0007)。beta フラグは公式なら全部通るので、既定は素通し。
///
/// 枠は応答ヘッダに載るので、まだ観測が無い状態は「取れるがまだ」と言える。
pub fn official(name: &str, wire: Arc<AnthropicWire>) -> Preset {
    let quota_api = OauthUsage::new(wire.base_url());
    Preset::new(
        name,
        Arc::new(OauthBearer::new(name)),
        wire,
        Arc::new(AnthropicMetering),
    )
    .with_quota_api(Arc::new(quota_api))
    .with_negotiation(Arc::new(BetaFlags::new(beta::Policy::Passthrough)))
    .with_quota_support(Support::Unobserved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn wire() -> Arc<AnthropicWire> {
        Arc::new(AnthropicWire::new(
            "claude-personal",
            "https://api.anthropic.com",
            BTreeMap::new(),
        ))
    }

    /// 公式は枠照会 API を持つ。持たない preset との差は型に出る (DR-0014 §2)。
    #[test]
    fn official_has_a_quota_api() {
        let preset = official("claude-personal", wire());

        assert_eq!(preset.name(), "claude-personal");
        assert!(preset.quota_api().is_some());
        assert!(
            preset.negotiation().is_some(),
            "beta の交渉はどの経路でも要る"
        );
        assert_eq!(
            preset.quota_support(),
            Support::Unobserved,
            "枠は応答ヘッダに載るので、使えば見える"
        );
    }
}
