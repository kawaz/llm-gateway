//! llm-gateway core.
//!
//! クライアントは Anthropic Messages API を話す。gateway はモデル名から
//! upstream と認証情報を選び、最小限の加工をして転送する。
//!
//! 構成は DR-0002 を参照:
//! - [`router`] モデル名 + session キー → (backend, credential) の優先順位
//! - [`backend`] upstream ごとのアダプタ。Messages API 系はプロバイダで分岐
//! - [`credential`] token の取得とリフレッシュ。永続化はプラガブル
//! - [`relay`] 本文を流し終えた (途切れた) ところの記録
//! - [`stats`] 応答の usage を日ごとに積む (DR-0011)
//! - [`pricing`] モデルごとの単価表。コストは閲覧時に計算する

pub mod backend;
pub mod config;
pub mod credential;
pub mod discovery;
pub mod gateway;
pub mod pattern;
pub mod pricing;
pub mod relay;
pub mod router;
pub mod session;
pub mod stats;
pub mod usage;

pub use config::Config;
pub use gateway::Gateway;

mod error;
mod persist;

pub use error::{Error, Result};
