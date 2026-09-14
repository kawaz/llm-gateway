---
title: keepalive-absence-tests-can-pass-vacuously
status: open
category: task
created: 2026-09-14T17:02:46+09:00
last_read:
open_entered: 2026-09-14T17:02:46+09:00
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

# keepalive-absence-tests-can-pass-vacuously

## 概要

`cache/keepalive.rs` の「何も送られない」ことを確かめるテスト
(`a_live_conversation_never_fires` / `a_blocked_route_is_not_replayed` /
`pausing_drops_the_kept_conversation` 等) は、待つ相手が無いので
`tokio::time::advance` + `yield_now` のままになっている。予定の task が
そもそも走らなくても (= 実装が壊れて発火経路が死んでいても) 通ってしまう
余地がある。

## 背景

worker replay-stage-b が CI ubuntu 失敗の真因調査の副産物として発見
(2026-09-14)。送信を試みた回数 (postpone / skip も含めた「判断が走った」
回数) を数える口を FakeUpstream か Keepalive に持たせ、「判断は走ったが
送らなかった」を assert する形に閉じたい。v0.48.1 で `until_sent` /
`until_withdrawn` の待ち合わせに寄せた姉妹テストと同じ流儀にする。

## 受け入れ条件

- [ ] `a_live_conversation_never_fires` / `a_blocked_route_is_not_replayed` /
      `pausing_drops_the_kept_conversation` 等の「何も送られない」系テストが、
      判断が走った回数 (postpone / skip を含む) を観測してから assert する形に
      なっている
- [ ] 発火経路そのものが壊れて task が走らないケースで、上記テストが red に
      なることを確認済み
- [ ] v0.48.1 の `until_sent` / `until_withdrawn` 待ち合わせと同じ流儀に揃っている

## TODO

<!-- wip 時のみ -->
