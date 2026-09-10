---
title: 監督者クラッシュ後に子プロセスが残り新監督者とポート衝突する
status: open
category: bug
created: 2026-09-10T15:19:14+09:00
last_read:
open_entered: 2026-09-10T15:19:14+09:00
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

# 監督者クラッシュ後に子プロセスが残り新監督者とポート衝突する

## 概要

監督者 (supervisor) が SIGKILL / 異常終了すると、起動していた子 (unit) プロセスが残置される。次に起動した監督者が同じ unit を起こそうとすると、残置された旧子プロセスとポートが衝突し backoff に入る。

## 背景

DR-0028 決定 11 (2026-09-10 実物確認) で以下が判明している:

- `spawn()` は `kill_on_drop(false)` で子を起動しており、監督者プロセス終了時に子を道連れにしない
- 子の pid はメモリ上のみで管理され、永続化されていない
- プロセスグループごと畳む経路も現状ない
- 旧子の pid ベース自動引き取り (= 監督者再起動時に旧 pid を推測して回収) は決定 11 で明示的に否定されている。理由: pid は OS に使い回されるため、無関係プロセスを誤って kill する経路になりうる

関連: DR-0028、`docs/runbooks/2026-09-09-migrate-launchd-to-service.md`。

## 受け入れ条件

- [ ] `daemon status` が「登録簿にある unit だが、listen しているプロセスが自分の子ではない」状態を区別して表示する (= 対象ポートの listen 保持プロセスを lsof 相当で調べ、監督者配下の pid と一致するか確認)
- [ ] backoff に入った際のログから、原因が「ポートが (孤児プロセスに) 塞がれている」ことが読み取れる
- [ ] 孤児プロセスの畳み方 (`lsof -i :<port>` で特定 → kill) が runbook に記載されている

## TODO

<!-- wip 時のみ -->
