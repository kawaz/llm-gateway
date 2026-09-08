---
title: keepalive-foreign-standby-fires-immediately
status: open
category: bug
created: 2026-09-09T05:16:03+09:00
last_read:
open_entered: 2026-09-09T05:16:03+09:00
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

# keepalive-foreign-standby-fires-immediately

## 概要

v0.43.6 で keepalive が自己ループする (ping → 返信 → classifier → 即 ping、6 秒周期)。

## 背景

実測 2026-09-09 05:11 JST、llm-gateway セッション (fable 5.1、ns personal):

- ping の返信 (fable、origin main) は Applied で rearm される
- 直後に走る同セッションの classifier リクエスト (sonnet、origin main、本文に transcript 経由で同じ nonce を含む) が `keepalive: foreign` と判定される (nonce は返信で消費済み = 登録簿に無い)
- v0.43.6 の `standby()` が nonce から復号した horizon_end (≈ 今) を引き継いで**即座に**次の ping を発火する
- 8 周期観測、各周期 fable 1.3MB + sonnet 600KB

暫定対処: 両 config の `main = "keepalive"` を `main = "1h"` に変更して両機再起動 (2026-09-09 05:15 JST、修正後に戻す)。

原因候補 2 つ (両方直す):

1. 同一セッションが消費済み nonce を含む本文を送ってきた場合 (classifier / 続くターン) を Foreign と誤判定する — 消費済み nonce を一定期間「自分が出した」として覚える必要がある (v0.43.5 以前も Foreign 判定はしていたが standby が now+horizon で 57 分後発火だったため顕在化しなかった)
2. `standby()` は STANDBY_AFTER (57 分) より早く発火してはならない — 復号した horizon_end が STANDBY_AFTER より近いなら控えを作らない

関連: DR-0024 追補「合図の終わりは合言葉が持ち歩く」、issue archive keepalive-foreign-standby-regenerates-horizon。

## 受け入れ条件

- [ ] 消費済み nonce を含む同一セッションの後続本文が Foreign 誤判定されない
- [ ] `standby()` が STANDBY_AFTER (57 分) より早く次の ping を発火しない
- [ ] 暫定対処 (`main = "1h"`) を元 (`main = "keepalive"`) に戻す
