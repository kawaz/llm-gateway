//! gateway の汎用層。
//!
//! 上流が何であっても変わらない部品だけを置く。上流の種類や、そこで流れる
//! 中身の語彙は利用側の crate が持ち、ここへは持ち込まない (DR-0030 §1)。
//! 語彙が漏れていないことは `tests/vocabulary.rs` が確かめる。
//!
//! - [`pattern`] `*` だけを扱う名前の照合
//! - [`persist`] 書き手ごとのファイルを、途中の状態を読ませずに置く作法
//! - [`stats`] 日次の集計を書き手ごとのファイルに置き、読むときに合わせる器
//! - [`config`] 設定を土台に重ねる規則、パスの開き方、既定の置き場
//! - [`credential`] 認証情報の置き場 ([`credential::Persistence`]) と、その平文ファイル実装
//! - [`credential::refreshing`] 更新を束ねて使える状態で渡す窓口 ([`credential::refreshing::CredentialStore`])
//! - [`credential::time`] 認証情報が持つ時刻 (RFC 3339) の読み書き
//! - [`daemon`] 登録した台を起こして生かし続ける監督者と、その登録簿・言葉
//! - [`error`] この層のエラー
//! - [`events`] 起きたことを通し番号付きで見ている人へ流す口
//! - [`ns`] namespace の入口での認証 ([`ns::NsAuth::verify`] → [`ns::Principal`])

pub mod config;
pub mod credential;
pub mod daemon;
pub mod error;
pub mod events;
pub mod ns;
pub mod pattern;
pub mod persist;
pub mod stats;

pub use error::{Error, Result};
