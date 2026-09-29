---
title: daemon reload — 設定の reload を restart 無しで行う (監督者経由 + unit ごとの制御 socket)
status: open
category: design
created: 2026-09-30T07:54:16+09:00
last_read:
open_entered: 2026-09-30T07:54:16+09:00
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

# daemon reload — 設定の reload を restart 無しで行う (監督者経由 + unit ごとの制御 socket)

## 概要

設定の reload を restart 無しで行う `daemon reload [--unit …]` を足す (kawaz 合意 2026-09-30、DR 未起草)。

- 契機は明示の CLI。config の mtime 監視はしない (エディタの保存途中や、extends の土台と派生を順に書き換える途中を拾うため)
- 経路は 2 段
  1. CLI → 監督者: 既存の unix socket (DR-0028 決定 3) に `Request::Reload(Which)` を足す
  2. 監督者 → 子: 新設の **unit ごとの unix socket** (監督者だけが繋ぐ、HTTP と分離) で命令し、結果 JSON `{unit, ok, error}` を受ける
- 子の HTTP に `POST /llm-gateway/reload` を置く案は不採用。Caddy 越しに外から届く管理面に設定変更操作を置くことになるため。既存の無認証 `POST /llm-gateway/keepalive/pause` も同じ整理で監督者経路へ寄せる候補

## 設計の要点

- 原子性: `check` と同じ経路で全体を読んで検証し、通った時だけ `Arc<Config>` を差し替える。失敗は旧設定のまま + エラーを CLI に返す。走行中リクエストは掴んだ Arc で完走
- reload 不可の欄 (`server.listen`、store / stats の dir、監督者の unit 定義) が変わっていたら拒否して「restart が要る」と返す (黙って旧値で続けない)
- 設定に紐づく走行状態 (affinity は route 名、spend_down / pace_cap の窓も route 単位) は「同名は引き継ぎ、消えた名前は捨てる」を仮置き。実物を見て決める
- 契機の一覧 (config = 明示 reload、credential = 周期監視、keys_file = 検証時 stat) を MANUAL に 1 表で書く
- `daemon status` に on_disk と running の config 差を出すかは後続

## 背景

keys_file 切替と preserved thinking の検証マトリクスは restart で先に進める。その後 DR 起草 → 実装の順。

## 受け入れ条件

- [ ] DR を起草する
- [ ] `daemon reload [--unit …]` と `Request::Reload(Which)`、unit ごとの制御 socket を実装する
- [ ] 検証失敗時は旧設定のままエラーが CLI に返る
- [ ] reload 不可の欄の変更は「restart が要る」として拒否される
- [ ] MANUAL に契機の表を載せる
