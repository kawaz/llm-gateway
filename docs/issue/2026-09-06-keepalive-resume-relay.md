---
title: keepalive の自動解除を兄弟 gateway へ中継する
status: wip
category: task
created: 2026-09-06T14:42:49+09:00
last_read:
open_entered: 2026-09-06T14:42:49+09:00
wip_entered: 2026-09-06T14:43:48+09:00
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

# keepalive の自動解除を兄弟 gateway へ中継する

## 概要

pause は兄弟へ中継されるが、解除 (実リクエストによる自動 resume) は受けた instance でしか起きない。Caddy が 11301 優先なので 11302 が paused のまま残り、その会話は idle 中に 11301 が落ちても 11302 が keepalive を引き継がない (standby が停止に阻まれる)。転送には影響しない。

実測 2026-09-05 (v0.38.0): 12d3283d を pause → 両 instance の paused に載る → 実リクエストで 11301 のみ resume、11302 は paused のまま。

## 背景

keepalive-pause-per-session (2026-09-05-keepalive-pause-per-session.md) で pause の中継は実装済み。resume 側の中継が漏れていることが実機検証で判明した。

## 方針

解除も pause と同じ経路で兄弟へ中継する。内部用 `POST /llm-gateway/keepalive/resume` (`{session_id}`) を持ち、実リクエストで resume した instance が relay ヘッダ付きで兄弟へ送る。ccmsg から叩く口ではない (人の解除操作は持たない方針は変えない)。relay ヘッダ付きで受けた側はさらに配らない。

## 受け入れ条件

- [ ] 実リクエストで resume した instance が兄弟の `/llm-gateway/keepalive/resume` を relay ヘッダ付きで叩く (fire-and-forget、失敗は warn 1 行)
- [ ] relay ヘッダ付きの resume は再配布しない
- [ ] resume で兄弟側の paused からも消える (実機: pause → 実リクエスト → 両 instance の paused が空)
- [ ] DR-0024 追補の該当箇所 (解除の伝搬) を更新
