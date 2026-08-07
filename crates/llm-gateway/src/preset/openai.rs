//! OpenAI Responses API を話す preset。

mod auth;
mod request;
mod response;
mod wire;

pub use auth::ChatGptBearer;
pub use wire::OpenAiWire;

use std::sync::Arc;

use crate::provider::Preset;
use crate::quota::Support;

/// ChatGPT サブスクの Codex backend へ出る preset。
pub fn chatgpt(name: &str, wire: Arc<OpenAiWire>) -> Preset {
    Preset::new(
        name,
        Arc::new(ChatGptBearer::new(name)),
        wire,
        Arc::new(crate::preset::anthropic::AnthropicMetering),
    )
    .with_quota_support(Support::Unobserved)
}
