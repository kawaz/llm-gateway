---
title: upstream の本文内エラー透過を適切な HTTP エラーに写像する
status: open
category: task
created: 2026-08-12T11:04:31+09:00
last_read:
open_entered: 2026-08-12T11:04:31+09:00
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

# upstream の本文内エラー透過を適切な HTTP エラーに写像する

## 概要

upstream の本文内エラー透過を適切な HTTP エラーに写像する。現象: Codex backend は context window 超過等のエラーを HTTP 200 のまま本文内 SSE で返す。全経路 denial 時に gateway は「最後に断られた応答をそのまま返します」で 200 のまま透過するため、client には「API returned an empty or malformed response (HTTP 200)」に見えて原因が伝わらない (実測 2026-08-12T01:26-01:27Z、kuu セッションの codex worker が context 超過で連続死)。

## 背景

改善案: ResponseAdmission が denial と判定した応答を透過する際、判別できたエラー種別 (context 超過等) に応じて Anthropic 形式の error JSON + 適切な HTTP status (context 超過なら 400 系) へ写像する。

関連: DR-0014 §9 (ResponseAdmission)、DR-0009 (denial 透過)。

## 受け入れ条件

- [ ] {完了の判定基準}

## TODO

<!-- wip 時のみ -->

- [ ] {次に手を付けるサブタスク}
