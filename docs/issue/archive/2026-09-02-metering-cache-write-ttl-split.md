---
title: metering が cache write の 1h/5m TTL 内訳を拾えていない
status: resolved
category: bug
created: 2026-09-02T14:04:03+09:00
last_read:
open_entered: 2026-09-02T14:04:03+09:00
wip_entered: 2026-09-06T15:40:08+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-06T15:56:34+09:00
discard_reason:
pending_reason:
close_reason: ["implemented","dr/DR-0014","change tttxtywk: TokenKind に cache_creation の 1h/5m 内訳を追加、Pricing の refines 親子関係で内訳の単価差分を課金、stats に w_1h/w_5m 列追加"]
blocked_by:
origin: 自リポ TODO
---

# metering が cache write の 1h/5m TTL 内訳を拾えていない

## 概要

metering が usage の `cache_creation.ephemeral_1h_input_tokens` /
`ephemeral_5m_input_tokens` の内訳を拾っておらず、合計の
`cache_creation_input_tokens` のみを扱っている (cache-tweaks worker 調査、
2026-09-02)。1h TTL のキャッシュ書き込みは input の 2 倍のコストだが、現状は
5m 単価 (1.25 倍) で USD 換算されているため過小評価になる。

## 背景

直すには 1h/5m の token kind を additive に (合算した既存の
`cache_creation_input_tokens` を壊さない形で) 追加し、stats JSON と pricing
ロジックを分離する必要がある。根拠は公式 prompt-caching doc (1h TTL は
input の 2 倍、5m TTL は 1.25 倍という単価差)。

Claude Code が 1h TTL を使い始めるまでは実害なし (低優先)。

## 受け入れ条件

- [ ] usage の `cache_creation.ephemeral_1h_input_tokens` /
      `ephemeral_5m_input_tokens` を metering が個別に集計する
- [ ] 1h TTL 書き込みが 2 倍単価、5m TTL 書き込みが 1.25 倍単価で USD 換算される
- [ ] stats JSON のスキーマと pricing ロジックが分離され、既存の合計値
      (`cache_creation_input_tokens`) との後方互換が保たれる
