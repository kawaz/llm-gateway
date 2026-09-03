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
mod origin;
mod quota_api;
mod wire;

use std::sync::Arc;

pub use auth::OauthBearer;
pub use metering::AnthropicMetering;
pub use negotiation::BetaFlags;
pub use origin::MetadataOrigin;
pub use quota_api::OauthUsage;
pub use wire::AnthropicWire;

use crate::provider::Preset;
use crate::quota::Support;

/// 枠の呼び名から、その枠が回る周期 (秒) を起こす (DR-0018 §6)。
///
/// Anthropic は窓の長さを数値では返さない。返るのは呼び名だけで、応答ヘッダ
/// なら欄名 (`...-5h-...` / `...-7d-...`)、枠照会 API なら `kind`
/// (`session` / `weekly_all` / `weekly_scoped`) がそれにあたる。周期が
/// 呼び名に埋まっているのはこの provider の事情なので、対応表はここに閉じる。
///
/// 知らない呼び名は `None`。推測で周期を埋めると、使い切りの判定 (DR-0018)
/// が誤った窓長を基準にしてしまう。
fn window_seconds(kind: &str) -> Option<u64> {
    const HOUR: u64 = 60 * 60;
    match kind {
        "5h" | "session" => Some(5 * HOUR),
        "7d" => Some(7 * 24 * HOUR),
        // 週次の枠は掛かる範囲 (全体 / モデル別) が違うだけで、周期は同じ。
        kind if kind.starts_with("weekly") => Some(7 * 24 * HOUR),
        _ => None,
    }
}

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
    .with_caller_origin(Arc::new(MetadataOrigin))
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
            "beta negotiation is required on every route"
        );
        assert_eq!(
            preset.quota_support(),
            Support::Unobserved,
            "quota rides on the response headers, so using it makes it visible"
        );
    }
}
