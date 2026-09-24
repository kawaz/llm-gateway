---
title: ns 認証 token 方式の固定文字列比較が定数時間でない
status: open
category: bug
created: 2026-09-24T16:05:18+09:00
last_read:
open_entered: 2026-09-24T16:05:18+09:00
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

# ns 認証 token 方式の固定文字列比較が定数時間でない

## 概要

`crates/gateway-core/src/ns.rs:73` 付近の token 方式 (`auth_token`) の照合は通常の文字列比較で、定数時間比較 (`subtle::ConstantTimeEq` 等) になっていない。理論上はタイミング差で token を 1 文字ずつ推測できる。段 1c の移動前からの実装で、jwt 方式 (Ed25519 の `verify_strict`) には当てはまらない。

## 背景

2026-09-24 の手順 4 (jwt) レビュー (reviewer-sol-high) の指摘。localhost / tailnet 内の利用でネットワーク jitter に埋もれる程度だが、直すコストは小さい (`subtle` crate は ed25519-dalek 経由で既に依存木にある)。

## 受け入れ条件

- [ ] token 方式の照合を定数時間比較にする (長さ差の扱いも含めて)
- [ ] 既存の `locked` / `tokenless` の試験が同じ名前で緑
