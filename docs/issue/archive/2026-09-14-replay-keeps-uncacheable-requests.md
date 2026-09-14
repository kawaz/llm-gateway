---
title: replay がキャッシュに乗らない request を保持し、55 分ごとに全量入力で自送信し続ける
status: resolved
category: bug
created: 2026-09-14T14:29:57+09:00
last_read:
open_entered: 2026-09-14T14:29:57+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-14T15:07:21+09:00
discard_reason:
pending_reason:
close_reason: ["dr/DR-0027","implemented","commit/xukymxol","research/2026-09-14-claude-code-uncached-requests"]
blocked_by:
origin: 自リポ TODO
---

# replay がキャッシュに乗らない request を保持し、55 分ごとに全量入力で自送信し続ける

## 概要

replay がキャッシュに乗らない request を保持し、55 分ごとに全量入力で自送信し続ける。

## 背景

実測 (2026-09-12〜14、unstable.log の `replayed the last request` 348 本): 系列は「毎回 hit」(267 本) と「毎回 none」(81 本、4 系列: 48b85fe7 ×23 / 96c83ec2 ×23 / 7d121f2e ×15 / 78937d64 ×5、他に gpt 系) に二分される。none = usage はあるが cache read も cache creation も 0 で、入力が全量非キャッシュとして上流に送られ、キャッシュも作られない。none 系列の保持 request (`stats/keepalive/49a66dfd-….7d121f2e.json`、2.1 MB) には `cache_control` が 1 つも無い (hit 系列 c88343cf の 776 KB には 3 つ)。

原因: `gateway.rs` の `keeping` (strategy == Replay) と `keep_for_replay` (2xx + `carries_tools`) はどちらも応答の usage を見ておらず、`events::Cache::of(usage)` が None の request も保持する。さらに replay の結果が none でも系列を止めない。

対処案:

1. 初回応答の `Cache::of` が Hit/Written/Partial の時だけ保持する (None/Unknown は保持しない、理由を debug ログ)
2. 自送信の結果が None なら系列を落とす (`cache_expired` 相当の event を出すかは DR-0012 と要整合)

DR-0027 の目的 (prompt cache の延命) からして、キャッシュが無い request を送り直す意味は無い。

費用影響: none 66 本 × ~50 万トークン級の入力が OAuth サブスク枠を消費している。

## 受け入れ条件

- [ ] `Cache::of` が None/Unknown の応答を返した request は replay 保持対象から除外される
- [ ] 保持中の系列が自送信で None を返した場合、その系列を打ち切る (DR-0012 との整合を確認した上で)

## TODO

<!-- wip 時のみ -->
