---
title: 全経路が断られた転送で cache_notice の約束が取り消されない
status: open
category: bug
created: 2026-09-14T17:02:44+09:00
last_read: 2026-09-23T18:39:41+09:00
open_entered: 2026-09-14T17:02:44+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: worker replay-stage-b の報告
---

# 全経路が断られた転送で cache_notice の約束が取り消されない

## 概要

全経路が断られた転送 (2xx が 1 本も返らない) では、各試行の request event が連鎖の欄と `cache_notice` を載せる一方で、控えは作られず (`keeping` は 2xx で絞る) `cache_expired` も出ないため、見る側 (webui のリング) が果たされない約束を持ったままになる。

## 背景

v0.48.0 (DR-0027 決定 8 の「1 本目から見立てを出す」) で顕在化した性質だが、`cache_expires_at` / `cache_notice` が status に関わらず載る挙動自体は以前から同じ。

直すなら以下のどちらかの裁定が要る:

1. 2xx でない試行では寿命を約束しない (連鎖の欄と `cache_notice` を載せない)
2. 断られた試行の直後に `cache_expired` で取り消す

統括の推し: 1 (断られた 1 本には延ばす cache が無いので約束しないのが自然、DR-0012 の request event の記述を 1 行直すだけ)。

発見: worker replay-stage-b の報告 (2026-09-14)。

## 受け入れ条件

- [ ] 全経路が断られた転送で、果たされない `cache_notice` / `cache_expires_at` が webui のリングに残らないことを確認する
- [ ] 上記 2 案のどちらを採るか裁定し、DR-0012 の request event の記述を更新する

## TODO

<!-- wip 時のみ -->

- [ ] {次に手を付けるサブタスク}
