//! gateway の汎用層。
//!
//! 上流が何であっても変わらない部品だけを置く。上流の種類や、そこで流れる
//! 中身の語彙は利用側の crate が持ち、ここへは持ち込まない (DR-0030 §1)。
//! 語彙が漏れていないことは `tests/vocabulary.rs` が確かめる。
//!
//! - [`pattern`] `*` だけを扱う名前の照合
//! - [`persist`] 書き手ごとのファイルを、途中の状態を読ませずに置く作法
//! - [`credential`] 認証情報の置き場 ([`credential::Persistence`]) と、その平文ファイル実装
//! - [`credential::refreshing`] 更新を束ねて使える状態で渡す窓口 ([`credential::refreshing::CredentialStore`])
//! - [`credential::time`] 認証情報が持つ時刻 (RFC 3339) の読み書き
//! - [`error`] この層のエラー
//! - [`ns`] namespace の入口での認証 ([`ns::NsAuth::verify`] → [`ns::Principal`])

pub mod credential;
pub mod error;
pub mod ns;
pub mod pattern;
pub mod persist;

pub use error::{Error, Result};
