---
title: local-issue 0.2.12 の close 動作を確認するための検証用 issue
status: resolved
category: task
created: 2026-08-02T07:31:12+09:00
last_read:
open_entered: 2026-08-02T07:31:12+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-08-02T07:32:13+09:00
discard_reason:
pending_reason:
close_reason: ["done: plugin v0.2.12 の close フロー検証が目的の一時 issue。検証が完了したので閉じる。実課題ではない。"]
blocked_by:
origin: 自リポ TODO
---

# local-issue 0.2.12 の close 動作を確認するための検証用 issue

## 概要

これは claude-local-issue plugin v0.2.12 の close フロー(archive 移動時に旧 path の削除が commit に含まれるか)を実機確認するためだけに作った一時的な issue。確認が済んだら即 close する。実際の課題ではないので、内容に意味はない。

## 背景

claude-local-issue plugin v0.2.12 の close 動作 (archive 移動時の旧 path 削除が commit に含まれるか) を実機確認する目的で作成。

## 受け入れ条件

- [ ] close 実行後、archive/ への移動と旧 path の削除が commit に含まれていることを確認する
