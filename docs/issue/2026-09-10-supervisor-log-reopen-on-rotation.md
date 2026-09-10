---
title: 監督者のログ fd をログローテーション後に開き直す
status: open
category: request
created: 2026-09-10T15:19:11+09:00
last_read:
open_entered: 2026-09-10T15:19:11+09:00
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

# 監督者のログ fd をログローテーション後に開き直す

## 概要

監督者が unit のログ (`logs/<unit>.log`) の追記 fd を子の生存中ずっと握るため、
newsyslog / logrotate の rename 方式の回転が次の `daemon restart` まで効かない
(DR-0028 決定 10 に制約を明記、runbook に newsyslog の例を記載済み)。

## 背景

方針案:

- (a) 監督者が SIGHUP でログを開き直す、または書き先の inode 変化を検知して
  開き直す (`supervisor.rs` の `pump()`)
- (b) newsyslog エントリから `N` フラグを外して pid ファイル経由で合図を
  送れるようにする
- (c) 回転直後に新ファイルへ書かれることを実機確認

現状でもファイルが伸びるだけで壊れないので優先度は低い。

## 受け入れ条件

- [ ] ログローテーション後、監督者が新しいログファイルへ書き込みを継続する
      (次の `daemon restart` を待たずに)
- [ ] 実機での動作確認 (newsyslog / logrotate いずれかの回転を発生させて確認)

## TODO

<!-- wip 時のみ -->

- [ ] {次に手を付けるサブタスク}
