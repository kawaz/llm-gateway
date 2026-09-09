//! この端末で走らせる gateway プロセスの持ち物 (DR-0028)。
//!
//! - [`registry`] どの設定を 1 台として走らせるかの登録簿
//! - [`supervisor`] 登録された台を抱えて生かし続ける監督者
//! - [`protocol`] 監督者に頼むときの言葉

pub mod protocol;
pub mod registry;
pub mod supervisor;
