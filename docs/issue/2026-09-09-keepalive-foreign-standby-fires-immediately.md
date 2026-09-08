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

タイトルの「6 秒周期の自己ループ」は誤判定だった。実際の周期は 55 分で正しく配送されていたが、統括セッションが idle 中に Monitor 通知を受け取らず起床時に溜まった分をまとめて処理したため短周期に見えた。ただし誤判定の元になった Foreign 判定の不具合自体は実在する (タイトルの「fires-immediately」は誤りだが slug は維持)。

## 背景

ccmsg daemon.log (`cache keepalive <nonce> … delivered`) で確認したところ、8 通の ping は 13:46〜20:11 UTC の間 55 分間隔で正しく配送されていた。統括セッションが idle 中に Monitor 通知を受け取らず、20:11 に起きた時点で溜まっていた 7 通を 1 ターンずつ順に処理したため、gateway 側では「12 秒周期の返信」に見えただけ。

暫定対処 (`main = "1h"`) は 2026-09-09 05:35 JST に `main = "keepalive"` へ戻して両機再起動済み。

残る実在の不具合:

- 同一セッションの後続リクエスト (classifier 等) が消費済み nonce を transcript 経由で含み `foreign` と誤判定され、その系列に控え (standby) が立つ
- v0.43.6 以前から続く二重 gateway の連鎖の一因 (issue archive keepalive-foreign-standby-regenerates-horizon)

worker が対処を実装済み (change wvwzyrvu):

- `Marker::Spent`: 消費済み nonce をその連鎖の終わりまで記憶 (上限 1024)
- standby の防波堤: horizon_end が now + STANDBY_AFTER 以内なら控えを作らない

関連: DR-0024 追補「合図の終わりは合言葉が持ち歩く」、issue archive keepalive-foreign-standby-regenerates-horizon。

## 受け入れ条件

- [ ] 消費済み nonce を含む同一セッションの後続本文が Foreign 誤判定されない (change wvwzyrvu の効果を確認)
- [ ] `standby()` が STANDBY_AFTER (57 分) より早く次の ping を発火しない (change wvwzyrvu の効果を確認)
- [x] 暫定対処 (`main = "1h"`) を元 (`main = "keepalive"`) に戻す (2026-09-09 05:35 JST 実施済み)
