---
title: replay の自送信が stats.record() にミリ秒を渡し日付が壊れる
status: wip
category: bug
created: 2026-09-12T20:02:14+09:00
last_read:
open_entered: 2026-09-12T20:02:14+09:00
wip_entered: 2026-09-12T20:23:12+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: DR-0027 段階 A の hit 率確認中に発見
---

# replay の自送信が stats.record() にミリ秒を渡し日付が壊れる

## 概要

replay の自送信が `stats.record()` にミリ秒を渡し、日付が 58667 年になる。`gateway.rs:1114` (`keep_for_replay` 後の応答記録) が `sent_at_ms` をそのまま `Stats::record(at)` に渡しているが、`record` → `credential::time::local_date(unix)` は秒を受ける (通常経路 `exchange.rs::observe` の `at` は秒)。

## 背景

結果として `~/.local/state/llm-gateway/stats/` に `58667-10-30.127-0-0-1-11301.json` のような日付のファイルが自送信 1 本ごとに生え (86.4 秒相当で「1 日」進む)、2026-09-12 時点で 200 件超発生している。`llm-gateway stats --unit unstable` の出力にもこれらが未来日付として全部混ざる。

発見経緯: DR-0027 段階 A の hit 率確認中 (cache_w=0 / cache_r 大で hit 自体は良好)。

## 受け入れ条件

- [ ] `gateway.rs:1114` で `sent_at_ms` を秒に変換してから `Stats::record(at)` に渡す (`to_unix_secs(sent_at_ms)`)
- [ ] `record(at: i64)` が単位を型で持たない設計課題に対応 (crate 内に秒/ミリ秒を区別する型があれば揃える、なければ検討)
- [ ] 既存の壊れた日付ファイル (`~/.local/state/llm-gateway/stats/` 配下、200 件超) を復元する移行手順を用意する。壊れた日付 D の days-since-epoch × 86400 / 1000 が実時刻の秒に相当し、日単位の精度は十分なので、正しい日付ファイルへマージして消す
- [ ] 修正後、新規の自送信で正しい日付にファイルが生成されることを確認する

## TODO

- [ ] worker `replay-stats-fix` (opus5-medium) に修正実装を委譲済み。進捗確認・レビュー
- [ ] stable 11302 の brew 反映後の再起動を確認し、両 unit 反映後に close する

## 進捗 (2026-09-12)

v0.46.1 で修正 (commit eb27cba3 + Release a34e72b2)。unstable 11301 は再起動済みで起動時修復により 219 件を吸収、5 桁年ファイル 0 件を実機確認。stable 11302 は brew 反映後の再起動待ち。両 unit 反映後に close する。
