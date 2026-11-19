---
title: メンテ終了になった humantime を置き換える
status: open
category: task
created: 2026-11-19T19:40:00+09:00
origin: CI の cargo-audit の警告 (2026-11-19、v0.73.3)
---

# メンテ終了になった humantime を置き換える

## 概要

CI の cargo-audit が `humantime` 2.4.0 を unmaintained として警告した (RUSTSEC-2026-0542、2026-09-14)。理由は、humantime が包んでいる chrono がメンテ終了になったこと。今は警告扱いなので CI は落ちない。脆弱性ではない。

## 使っている所

`crates/llm-gateway/src/config.rs` だけで、設定の時間の値 (`30s`、`1h 30m` のような表記) を読み書きしている。

- `humantime::parse_duration`: 設定の読み込み
- `humantime::format_duration`: 設定の書き出しと案内文

依存は `llm-gateway` crate からの直接の依存だけで、他の crate を経由して入ってはいない (`cargo tree -i humantime`)。

## やること

- [ ] 代わりの crate を選ぶか、自前で読み書きするかを決める。設定に書ける表記 (複合表記を含む) が変わらないことを条件にする
- [ ] 置き換えて、既存の設定の試験がそのまま通ることを確かめる
- [ ] `cargo audit` の警告が消えたことを確かめる
