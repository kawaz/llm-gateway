---
title: core crate の日本語 error message 残件 (DR-0008 続き)
status: open
category: task
created: 2026-08-12T00:05:18+09:00
last_read:
open_entered: 2026-08-12T00:05:18+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: 自リポ TODO
---

# core crate の日本語 error message 残件 (DR-0008 続き)

## 概要

`crates/llm-gateway/src/{egress,gateway,exchange,discovery,webhook}.rs` に
`Error::Config` / `Err(format!(...))` 形の日本語 error message が複数残っている。

例:
- `webhook.rs:208` `"{status} が返りました"`
- `discovery.rs:91` `"一覧を読めません: {e}"`

tracing ログの日本語は対象外 (error message のみ)。

## 背景

2026-08-12 の CLI 英語化 (issue `2026-07-30-dr-0008-cli-language-mixing`) で
`main.rs` / `error.rs` / `config.rs` は英語化済み。残りの core crate 側は
DR-0008 の「触ったついでに直す」運用方針で対応する (一括置換ではなく、
該当ファイルを触る変更のついでに英語化する)。

## 受け入れ条件

- [ ] `egress.rs` / `gateway.rs` / `exchange.rs` / `discovery.rs` / `webhook.rs`
      の `Error::Config` / `Err(format!(...))` 形のユーザ向け error message が
      すべて英語化されている (tracing ログは対象外)

## TODO

<!-- wip 時のみ -->
