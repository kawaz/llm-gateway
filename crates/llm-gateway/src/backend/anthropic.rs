//! Messages API を話す upstream の呼び出し互換 (facade)。
//!
//! 実体は [`crate::preset::anthropic`] 以下の preset へ移った。router と
//! gateway が preset を直接扱うようになるまでの通り道として、[`Provider`] と
//! 転送の入口だけを残す。ここに判断は無く、preset の部品へ委ねる。

pub mod forward;
pub mod provider;

use serde_json::Value;

use crate::Result;
use crate::credential::Credential;
use crate::provider::Preset;

pub use crate::egress::Headers;
pub use crate::preset::anthropic::{beta, model_of, rewrite_model};
pub use provider::{Bedrock, Official, Relay};

/// 1 経路の upstream。
pub trait Provider: Send + Sync {
    /// ログや失敗記録に出す名前。
    fn name(&self) -> &str;

    /// 転送先の根 (`/v1/messages` などを足す前)。
    fn base_url(&self) -> &str;

    /// 認証ヘッダを載せる。方式は upstream ごとに違う。
    fn authorize(&self, headers: &mut Headers, credential: Option<&Credential>) -> Result<()>;

    /// この upstream に送るときの beta の既定。
    ///
    /// 適用は転送側が行う。credential が学習した拒否リストを足したうえで、
    /// 何を載せたかを控え、400 なら落として送り直す — という一連が provider
    /// 1 つでは完結しないため (DR-0003)。
    fn beta_policy(&self) -> beta::Policy;

    /// リクエストを upstream の要求に合わせる。
    fn adapt(&self, body: &mut Value, headers: &mut Headers);

    /// 小 trait の束。転送 ([`forward::send`]) はこちらを通る。
    fn preset(&self) -> &Preset;
}
