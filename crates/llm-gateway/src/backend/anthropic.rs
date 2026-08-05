//! Messages API を話す upstream の呼び出し互換 (facade)。
//!
//! 判断は [`crate::preset::anthropic`] 以下の preset にある。ここに残るのは
//! 呼び出し側が持っている形 (`Value` のボディ + ヘッダの組) と preset の
//! 契約を繋ぐ通り道と、その入口が使う名前の再輸出だけ。

pub mod forward;

pub use crate::egress::Headers;
pub use crate::preset::anthropic::{beta, model_of, rewrite_model};
