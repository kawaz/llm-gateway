//! upstream との通信の呼び出し互換 (facade)。
//!
//! 実体は [`crate::preset`] の provider preset にある。router と gateway が
//! preset を直接扱うようになるまで、旧アダプタの名前と引数の形をここが保つ。

pub mod anthropic;
