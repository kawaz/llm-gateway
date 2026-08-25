---
title: DR-0021 status API レビュー Minor 指摘 7 件
status: open
category: tech-memo
created: 2026-08-25T14:23:19+09:00
last_read:
open_entered: 2026-08-25T14:23:19+09:00
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

# DR-0021 status API レビュー Minor 指摘 7 件

## 概要

DR-0021 status API レビュー (fable5-high、2026-08-25) で見つかった Minor 指摘 7 件の記録。いずれも実害小、次弾でまとめて対処可。

1. `observed_from` の同秒 failure が success に隠れる非対称 (秒粒度限界)
2. link source が failure trigger 時に `at=now` の合成 unknown snapshot を得て stale 計算対象になる (DR では link は placeholder 扱い)
3. route から参照されない configured source が report から消える (operator が気づきにくい)
4. source なし route の `official.source="none"` / `source_url=""` が DR 語彙外 (schema 文書化要)
5. `official_from` の空 id 分岐は config validation により到達不能なデッドコード
6. single-flight テスト `concurrent_refreshes_share_one_fetch_and_result` に理論的 race (`hits==2` 断定)、および `observe_success` が 2xx のみで採用透過される 4xx を reachable に数えない保守的 drift
7. CLI status の UPDATED 列が直近更新時に「just now ago」と表示される (just now に ago が重複)。format_age 相当の表示関数の cosmetic bug。実機 v0.25.0 で確認 (2026-08-25)。

## 背景

DR-0021 status API のレビュー (fable5-high effort、2026-08-25 実施) で洗い出された。いずれも Minor 判定で実装のブロッカーではないが、放置すると schema 文書と実装の乖離や、テストの理論的な脆さが積み残しになる。

## 受け入れ条件

- [ ] 7 件それぞれについて対処要否を判断し、対処するものは実装・schema 文書・テストへ反映する
- [ ] 対処しない項目は理由を明記して close する
