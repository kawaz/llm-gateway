//! upstream との通信。
//!
//! アダプタは「どの API を話すか」で分ける。Anthropic Messages API を話す
//! upstream は転送と SSE 中継の実体を共有し、接続先・認証・リクエストの
//! 微調整だけを [`AnthropicProvider`] の実装が持つ (DR-0002)。

pub mod anthropic;

// Phase 2: OpenAI Responses API を話すアダプタ。Messages との相互変換を持つ。
// pub mod openai;
