---
title: request event に keepalive の終端 (keepalive_until) を載せ、pause 時に event を出す
status: resolved
category: request
created: 2026-09-06T19:03:13+09:00
last_read:
open_entered: 2026-09-06T19:03:13+09:00
wip_entered: 2026-09-06T19:04:14+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-06T19:46:18+09:00
discard_reason:
pending_reason:
close_reason: ["implemented:change oqlqyprp", "done:仕様拡張 (時刻欄を Unix ms 整数に統一、_iso 全廃、request event に cache_since/cache_count/next_keepalive_at/cache_until/cache_until_count/cache_breakeven_until/cache_breakeven_count/cache_paused を追加、keepalive_paused を cache_paused に置換、pause 時 type=keepalive_paused event。cache_until は horizon を跨いだ最後の1本まで出る実挙動と同じ計算)", "dr/DR-0012:implemented", "dr/DR-0024:implemented", "done:MANUAL 更新", "done:ccmsg には r276 で新形式を通知済み", "related:api-timestamps-unix-ms (usage/status/stats の ms 化は別 issue)"]
blocked_by:
origin: 自リポ TODO
---

# request event に keepalive の終端 (keepalive_until) を載せ、pause 時に event を出す

## 概要

ccmsg のリングタイマーが cache_expires_at (1h) しか知らないので、keepalive で延命される終端を渡す。kawaz 2026-09-06。

## 背景

- 設計 (裁定済み):
  - request event に `keepalive_until` / `keepalive_until_iso` を追加。値は **最後に出る合図 + cache TTL** (= 55 分刻みで horizon_end を超えない最後の合図の時刻 + 3600)。連続値の horizon_end や ratio は載せない (UX に意味があるのは離散化した終端だけ)
  - keepalive 戦略が効く main 系列にだけ付け、無い時は欄ごと出さない (既存の keepalive / keepalive_paused と同じ流儀)
  - 見込み値: 経路が落ちて合図が出せない / late で 1h が付かない場合は実際より早く切れる。ccmsg は最新 event で上書きする前提
  - pause 時に `kind: "keepalive_paused"` の event (session_id, paused_at) を出す (兄弟からの relay で受けた側は出さない、二重通知防止)。解除は実リクエストの request event に keepalive_paused: false が載ることで伝わるので、解除直後の 1 件は false を明示する (常時載せるかは ccmsg 側の扱いやすさで決める)
  - DR-0012 (events) と DR-0024 追補に欄を追記

## 受け入れ条件

- [ ] request event に keepalive_until (+_iso) が keepalive 適用時のみ載る。値は last_fire + ttl
- [ ] pause API で kind=keepalive_paused の event が webhook / SSE に流れる (relay 受信側は出さない)
- [ ] 解除直後の request event に keepalive_paused: false が載る
- [ ] DR-0012 / DR-0024 更新、ccmsg 側へ通知 (claude-ccmsg リポ issue keepalive-pause-button に追記依頼)
