//! OpenAI Responses API を話す preset。

mod admission;
mod auth;
mod metering;
mod quota_api;
mod request;
mod response;
mod wire;

pub use admission::OpenAiResponseAdmission;
pub use auth::ChatGptBearer;
pub use metering::OpenAiMetering;
pub use quota_api::WhamUsage;
pub use wire::OpenAiWire;

use std::sync::Arc;

use crate::provider::Preset;
use crate::quota::Support;

/// ChatGPT サブスクの Codex backend へ出る preset。
pub fn chatgpt(name: &str, wire: Arc<OpenAiWire>) -> Preset {
    let quota = WhamUsage::new(wire.base_url());
    Preset::new(
        name,
        Arc::new(ChatGptBearer::new(name)),
        wire,
        Arc::new(OpenAiMetering),
    )
    .with_response_admission(Arc::new(OpenAiResponseAdmission))
    .with_quota_api(Arc::new(quota))
    .with_quota_support(Support::Unobserved)
}
