//! この端末で走らせるプロセスの持ち物 (DR-0028)。
//!
//! - [`registry`] どの設定を 1 台として走らせるかの登録簿
//! - [`supervisor`] 登録された台を抱えて生かし続ける監督者
//! - [`protocol`] 監督者に頼むときの言葉
//! - [`port`] 台の待ち受けポートを今だれが握っているか
//!
//! 置き場 (状態ディレクトリ) と台への問い合わせ方は利用側が渡す。

pub mod port;
pub mod protocol;
pub mod registry;
pub mod supervisor;
