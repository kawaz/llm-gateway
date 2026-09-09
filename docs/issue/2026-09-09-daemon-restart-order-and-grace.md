---
title: daemon restart --all の順序と停止猶予を設定可能にする
status: open
category: request
created: 2026-09-09T16:32:04+09:00
last_read:
open_entered: 2026-09-09T16:32:04+09:00
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

# daemon restart --all の順序と停止猶予を設定可能にする

## 概要

`daemon restart --all` の (1) unit 停止順序 と (2) SIGTERM 後の強制終了猶予 (grace) を設定可能にする。

## 背景

実測 2026-09-09 16:31 JST (v0.44.1、ccmsg webhook 追加の rolling restart) で以下を観測:

1. **順序が登録の逆順で固定** (DR-0028 決定 4)。stable → unstable の順に `daemon add` した本番では、restart --all 時に unstable (11301、Caddy 優先) → stable (11302) の順に落ちた。全断は無いが、意図 (11302 を先に落としたい) と逆順になる。
   - 方針案: unit ごとに `priority` (登録簿 or config `[server]`) を持たせ、`restart --all` はその降順で処理。`daemon add --priority <n>` で指定可能にし、`daemon list` に表示。無指定時は登録順の逆 (現状の挙動) を維持。
2. **stable の `last_exit` が `signal: 9 (SIGKILL)`**。SIGTERM 送出後の猶予 10s で畳めず強制終了になっていた。in-flight の streaming 応答待ちが原因と推定 (未確認)。
   - gateway 側の graceful shutdown (新規受付停止 → in-flight 完了待ち) がどこまで実装されているか確認する。
   - 猶予を `stop_grace_secs` として設定可能にする。既定は現行 10s より長め (例 30s) を検討。

関連: DR-0028 決定 3・4。

## 受け入れ条件

- [ ] unit ごとに restart 順序を制御できる (`priority` 設定 + `daemon add --priority` + `daemon list` 表示)
- [ ] gateway の graceful shutdown 実装状況 (新規受付停止 → in-flight 完了待ち) を確認する
- [ ] SIGTERM 後の強制終了猶予を `stop_grace_secs` として設定可能にする
