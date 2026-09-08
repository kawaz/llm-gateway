---
title: origin 判定に請求ヘッダの cc_is_subagent=true を使う
status: open
category: request
created: 2026-09-08T14:11:08+09:00
last_read:
open_entered: 2026-09-08T14:11:08+09:00
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

# origin 判定に請求ヘッダの cc_is_subagent=true を使う

## 概要

origin (main/subagent/classifier 等) 判定に、請求ヘッダ
`x-anthropic-billing-header` 内の `cc_is_subagent=true` を使う。

## 背景

実測 2026-09-08 (v0.43.3、`claude -p` から Agent tool で Explore 起動を tap で捕捉):

- subagent の system[0] は `x-anthropic-billing-header: cc_version=2.1.263.2ad; cc_entrypoint=sdk-cli; cc_is_subagent=true;`
- main は `…9c1; cc_entrypoint=sdk-cli;`
- classifier は `…b6e; …`

`-p` 経路では `metadata.user_id` に `parent_session_id` が無いため DR-0024 の sub 判定
(parent_session_id ベース) が効かず、`oneshot` 規則で sub に落ちているだけだった。
hex 接尾辞も種別で変わっており、起動ごとのノイズだけではない。

方針案: origin 判定を `cc_is_subagent=true` → sub、を parent_session_id 判定と
併置する (どちらかが真なら sub)。DR-0024 の判定表に追記が必要。

cc_version の hex 接尾辞の意味は未解明 (未検証として findings に残す)。

## 受け入れ条件

- [ ] `cc_is_subagent=true` ヘッダによる sub 判定が parent_session_id 判定と併置される
- [ ] DR-0024 の判定表に本ケースが追記される
