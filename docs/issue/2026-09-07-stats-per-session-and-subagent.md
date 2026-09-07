---
title: stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望)
status: open
category: request
created: 2026-09-07T19:14:06+09:00
last_read:
open_entered: 2026-09-07T19:14:06+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: ccmsg
---

# stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望)

## 概要

ccmsg 統括からのフラグ (2026-09-07、r279m10、裁定不要の情報共有): subagent のモデル別コストを測ろうとして、gateway の stats が credential × model × 日 の粒度なので「同じ model の effort 違い」「親セッション vs subagent」を分離できない。

一次資料: claude-ccmsg リポ `docs/research/2026-09-07-eli5-bench/` (7 agent 並列の実験)。

## 背景

## 候補 (採否は gateway 側で判断)

- request / response event に subagent の識別を載せる (Claude Code が送る agent id 相当のヘッダ / 本文の印があるか要確認、origin=sub の下位区分)
- stats に session × model (× origin) の集計を持つ。DR-0011 の日次集計は credential × model なので、粒度を足すなら保存形式と閲覧 API (`/llm-gateway/stats`) の設計が要る
- effort は本文の thinking / reasoning 設定から判別できるが、記録に載せるなら metering 側で拾う

## 受け入れ条件

- [ ] Claude Code の subagent リクエストを識別できる材料 (ヘッダ / 本文) を tap で棚卸し
- [ ] 集計粒度の設計 (DR)
- [ ] ccmsg 側へ結果を返す
