//! llm-gateway core.
//!
//! クライアントは Anthropic Messages API を話す。gateway はモデル名から
//! upstream と認証情報を選び、最小限の加工をして転送する。
//!
//! 構成は DR-0002 / DR-0014 を参照:
//! - [`router`] モデル名 + session キー → 経路の優先順位。経路が使えるかは経路に聞く
//! - [`provider`] provider preset の契約 (Auth / Wire / Metering / 任意 capability) と経路の状態
//! - [`preset`] その契約の provider ごとの実装。方言と認証を組み合わせて束ねる
//! - [`egress`] upstream へ出るときの provider-neutral な HTTP の形
//! - [`backend`] 旧アダプタの呼び出し互換 (実体は [`preset`] へ移った)
//! - [`credential`] token の取得とリフレッシュ。永続化はプラガブル
//! - [`denial`] 1 経路の状態機構 — 締め出しの印・枠・様子見 (DR-0009)
//! - [`exchange`] 本文を流し終えた (途切れた) ところの記録
//! - [`events`] 転送のたびに起きたことを見ている人へ流す (DR-0012)
//! - [`webhook`] その知らせを受け口へ送る (DR-0012)
//! - [`quota`] 枠の観測スナップショットと、その置き場 (DR-0007)
//! - [`stats`] 応答の usage を日ごとに積む (DR-0011)
//! - [`pricing`] モデルごとの単価表。コストは閲覧時に計算する

pub mod backend;
pub mod config;
pub mod credential;
pub mod denial;
pub mod discovery;
pub mod egress;
pub mod events;
pub mod exchange;
pub mod gateway;
pub mod metering;
pub mod pattern;
pub mod preset;
pub mod pricing;
pub mod provider;
pub mod quota;
pub mod router;
pub mod session;
pub mod stats;
pub mod webhook;

pub use config::Config;
pub use gateway::Gateway;

mod error;
mod persist;

pub use error::{Error, Result};
