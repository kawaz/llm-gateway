---
title: OpenAI 変換が text 以外の system block で request 全体を断る
status: open
category: bug
created: 2026-11-06T15:34:27+09:00
last_read:
open_entered: 2026-11-06T15:34:27+09:00
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

# OpenAI 変換が text 以外の system block で request 全体を断る

## 概要

Claude Code の agent (reviewer-sol-high、gpt-6-sol) が `/code-review` skill を走らせた時、gateway の Anthropic → OpenAI 変換が `400 could not translate the request for this route: system may only contain text blocks` で request を断った (2026-11-06、第 ③ 段レビュー中に観測)。`crates/llm-gateway/src/preset/openai/request.rs` の `system_text` が `type != "text"` の system block を一律に拒否する。

Claude Code が system に text 以外の block を入れる場面があり、codex 系の経路では 1 request 全体が落ちて agent が途中終了する。

## 背景

DR-0025 (Responses ingress は無変換) ではなく、Anthropic Messages 形式を codex backend へ変換する経路の話。block の type は未確認。

## 調べること

- dump (DR-0038) で `--ns` を絞って取り、実際の system block の type を確認する
- 次のどれにするか決める: 無視して text だけ繋ぐ (dropped に数える) / type ごとに変換する / 断る

## 受け入れ条件

- [ ] 実際の system block の type が分かっている
- [ ] 扱い (無視 / 変換 / 断る) が決まり、実装とテストがある
- [ ] 該当 agent の `/code-review` が codex 経路で落ちない
