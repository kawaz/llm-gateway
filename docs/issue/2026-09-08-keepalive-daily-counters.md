---
title: keepalive ping の実発火回数を日別に永続化する
status: open
category: request
created: 2026-09-08T16:40:18+09:00
last_read:
open_entered: 2026-09-08T16:40:18+09:00
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

# keepalive ping の実発火回数を日別に永続化する

## 概要

keepalive ping の実発火回数を origin 別 (main / sub / keepalive) に日別で `llm-gateway stats` から確認できるようにする。

## 背景

`docs/findings/2026-09-08-keepalive-field-observation.md` の実運用評価で、keepalive の効果 (fable 5.1 の write 費用 / read 1M が 0.759 → 0.152) は stats から確認できたが、ping の発火回数そのものは永続化されていない。このため ping 費用の実額と収束の健全性を事後に検証できない。

DR-0027 (自送信 replay、2026-09-10 Accepted) 決定 6 により、自送信は request event に `origin: keepalive` で出て usage / stats に通常計上される。したがって専用カウンタ (`keepalive-counters/*.json` 等) は不要で、既存の stats 集計に origin 軸を足すだけで済む。旧方針案の `applied` / `late` / `foreign` は合図方式 (DR-0024) の語彙であり、replay 方式では消える。

findings の「追加計測案」3 点は、origin 軸での集計に置き換えられる範囲で正本とする。

関連: DR-0027, DR-0024, DR-0011

**DR-0027 段階 A の実装後に着手。**

## 受け入れ条件

- [ ] `llm-gateway stats` (または `/llm-gateway/stats`) が origin 別 (main / sub / keepalive) に日別の本数・in/out/cache_read/cache_creation トークン・USD を割って出せる
- [ ] `docs/findings/2026-09-08-keepalive-field-observation.md` の「追加計測案」3 点との整合を確認する
