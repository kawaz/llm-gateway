---
title: replay store の置き場が合図方式の見張り置き場と衝突している
status: open
category: bug
created: 2026-09-11T00:01:47+09:00
last_read:
open_entered: 2026-09-11T00:01:47+09:00
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

# replay store の置き場が合図方式の見張り置き場と衝突している

## 概要

DR-0027 段階 A の replay の置き場 `<stats.dir>/keepalive/<session>.<prefix>.json` が、合図方式の見張り置き場
`<stats.dir>/keepalive/127-0-0-1-<port>.json` と同じディレクトリになっている。このため起動時に replay store が
旧ファイル (合図方式のファイル) を読もうとして `the kept request is unreadable; dropping it` を 4 件 WARN する
(実測 2026-09-11 00:00 JST、v0.46.0)。削除はしていない (= drop は読み飛ばしであって破壊操作ではない)。

## 背景

DR-0027 段階 A で replay store を `<stats.dir>/keepalive/` 配下に追加したが、既存の合図方式の見張りファイルも
同じディレクトリに置かれていたため、起動スキャン時に両者が混在し、replay store が自分のものでないファイルを
「壊れた replay ファイル」として誤検出・WARN する状態になっている。

方針: replay の置き場を `<stats.dir>/keepalive/replay/` (または `replay/`) のようなサブディレクトリに分け、
起動スキャンで拾う対象を自分の命名規則 (`<session>.<prefix>.json`) に限定する。

段階 B で合図方式を撤去するタイミングで、旧ファイル (合図方式の見張りファイル) の掃除手順を runbook に記載する。

## 受け入れ条件

- [ ] replay store の置き場が合図方式の見張り置き場と分離されている
- [ ] 起動時に合図方式の旧ファイルを replay 候補として誤検出しない (WARN が出ない)
- [ ] 段階 B (合図方式撤去) 時の旧ファイル掃除手順が runbook に記載されている
