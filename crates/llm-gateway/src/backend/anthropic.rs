//! Anthropic Messages API を話す upstream。
//!
//! 転送と SSE 中継の実体はこのモジュールが持ち、upstream ごとの差は
//! [`AnthropicProvider`] の実装に閉じる。Bedrock は公式と同じ API を
//! 提供していて (SSE の形式まで同一)、違うのは接続先・認証方式・
//! モデル名・受け付ける beta フラグだけだった。

use serde_json::Value;
use url::Url;

use crate::Result;

pub mod beta;

/// Messages API を話す upstream ごとの差分。
pub trait AnthropicProvider: Send + Sync {
    /// ログや失敗記録に出る名前。
    fn name(&self) -> &str;

    /// 転送先。
    fn endpoint(&self) -> &Url;

    /// 認証ヘッダを載せる。方式は upstream ごとに違う。
    fn authorize(
        &self,
        headers: &mut Vec<(String, String)>,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// リクエストを upstream の要求に合わせる。
    ///
    /// 既定は素通し。公式 Anthropic はこれで足りる。Bedrock はモデル名を
    /// 自分の名前空間のものへ替え、受け付けない beta フラグを落とす。
    fn adapt(&self, _body: &mut Value, _headers: &mut Vec<(String, String)>) {}
}
