---
title: サブスク OAuth 経路は Claude Code 形の request のみ通す (429 "Error" の真因)
status: open
category: tech-memo
created: 2026-09-03T12:10:29+09:00
last_read:
open_entered: 2026-09-03T12:10:29+09:00
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

# サブスク OAuth 経路は Claude Code 形の request のみ通す (429 "Error" の真因)

## 概要

サブスク OAuth (`claude_oauth`) 経路は、Claude Code の形をしていない request を
429 `{"type":"rate_limit_error","message":"Error"}` で弾く。既知の「小 probe だけ
429 になる」現象の真因とみられる。

## 背景

実測 (2026-09-03):

- 素の本文 (Claude Code の形を持たない request) ×3 全滅 (429)
- system[0] に `x-anthropic-billing-header: cc_version=…; cc_entrypoint=cli;` +
  system[1] `You are Claude Code…` + `metadata.user_id` + `User-Agent: claude-cli/…`
  + `x-app: cli` + beta ヘッダを付けると、同一本文が 200

どの要素が必須条件かは未分解 (ヘッダだけ / system[0] だけ 等の切り分け未実施)。

## 受け入れ条件

- [ ] gateway 自身の quota probe (`usage?refresh=true` の最小 request) と疎通プローブが
      同じ形になっているか確認し、なっていなければ Claude Code 形に揃える
- [ ] 429 `Error` (本文が素の "Error") を「経路の rate limit」でなく「request 形状の拒否」
      として分類し、他 credential へフォールバックせず (全滅して 60 秒締め出しを誘発しない
      よう) 即 4xx で返す判定を検討する
- [ ] 必須条件の切り分け (ヘッダのみ / system[0] のみ 等) を実施し、最小構成を特定する

## TODO

<!-- wip 時のみ -->
