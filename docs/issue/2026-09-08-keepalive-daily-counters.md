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

keepalive ping の実発火回数 (`applied` / `late` / `foreign` 等) を日別に永続化する。現状の `count` 系メトリクスは生きている系列の現在値のみで、履歴は消えてしまう。

## 背景

`docs/findings/2026-09-08-keepalive-field-observation.md` の実運用評価で、keepalive の効果 (fable 5.1 の write 費用 / read 1M が 0.759 → 0.152) は stats から確認できたが、ping の発火回数そのものは永続化されていない。このため ping 費用の実額と収束の健全性を事後に検証できない。

方針案: 日次 stats と同じ流儀で `~/.local/state/llm-gateway/stats/` 相当に `keepalive-counters/<待ち受け>.json` (日別、writer 毎) を積み、`/llm-gateway/stats` か専用 endpoint で読めるようにする。

findings の「追加計測案」3 点を正本とする。

関連: DR-0024, DR-0011

## 受け入れ条件

- [ ] keepalive ping の `applied` / `late` / `foreign` 等の発火回数が日別・writer 別に永続化される
- [ ] 永続化された値が `/llm-gateway/stats` か専用 endpoint 経由で読める
- [ ] `docs/findings/2026-09-08-keepalive-field-observation.md` の「追加計測案」3 点との整合を確認する
