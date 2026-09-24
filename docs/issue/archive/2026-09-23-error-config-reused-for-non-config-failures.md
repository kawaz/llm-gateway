---
title: Error::Config が設定読み込み以外の失敗に流用されている
status: resolved
category: bug
created: 2026-09-23T18:21:09+09:00
last_read:
open_entered: 2026-09-23T18:21:09+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-24T16:29:56+09:00
discard_reason:
pending_reason:
close_reason: ["implemented:c741705c","dr/DR-0014#8","doc/MANUAL-error-table"]
blocked_by:
origin: 自リポ TODO
---

# Error::Config が設定読み込み以外の失敗に流用されている

## 概要

`Error::Config` の表示文言は `could not read the configuration: ...` だが、request の変換失敗以外にも設定と無関係な失敗に流用されている。利用者には設定が壊れているように見え、server 層のマッピングも `Config` 一括で 400 になるため、上流起因の失敗まで `invalid_request_error` に見える可能性がある。

## 背景

issue `codex-route-rejects-image-tool-result` の対応で request 変換失敗は `Error::UntranslatableRequest` に分離した。その worker が報告した残りの流用箇所 (2026-09-23 時点、裏取り前の worker 観察):

- `crates/llm-gateway/src/egress.rs:510` 付近 "request has no model"
- `egress.rs` / `preset/openai/response.rs` / `preset/openai/wire.rs` の SSE 読み取り失敗
- `quota_api.rs`
- `admission.rs`
- `exchange.rs` "response reading was interrupted"

## 受け入れ条件

- [ ] 上記各箇所を読み、「設定の問題 / request 本文の問題 / 上流応答の問題 / 内部の問題」のどれかに分類する
- [ ] 分類ごとに既存 variant (`UntranslatableRequest` / `Upstream` 系 等) へ寄せるか新 variant を切るかを決め、server 層の status マッピング (`crates/llm-gateway-server/src/lib.rs` の `error_response`) を分類に合わせる
- [ ] `Error::Config` は設定読み込みだけに使われている
