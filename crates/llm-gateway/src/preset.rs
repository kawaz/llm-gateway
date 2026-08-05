//! provider preset の実装置き場 (DR-0014)。
//!
//! core は [`crate::provider`] で小 trait と入出力の契約だけを規定し、どんな
//! provider が居るかを知らない。ここから下が「その IF の provider ごとの
//! impl」で、preset は Auth / Wire / Metering / 任意の QuotaApi を束ねたもの。
//!
//! 束ね方は 3 通りある。方言も認証も自前で持つもの ([`anthropic`])、認証だけ
//! 差し替えて方言を借りるもの ([`bedrock`])、認証を持たず方言だけ借りるもの
//! ([`relay`])。認証の軸と方言の軸が直交しているので、後の 2 つは Wire を
//! 書き直さずに設定だけで作れる。

pub mod anthropic;
pub mod bedrock;
pub mod relay;
