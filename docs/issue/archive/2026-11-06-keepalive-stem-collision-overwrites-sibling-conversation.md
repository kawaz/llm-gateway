---
title: keepalive の控えのファイル名 stem が会話どうしで衝突し、後から書いた方が上書きする
status: resolved
category: bug
created: 2026-11-06T14:59:09+09:00
last_read:
open_entered: 2026-11-06T14:59:09+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-11-08T15:41:10+09:00
discard_reason:
pending_reason:
close_reason: ["implemented","commit/557dd79709f9"]
blocked_by:
origin: 自リポ TODO (DR-0042 第 1 段の実装中に発見)
---

# keepalive の控えのファイル名 stem が会話どうしで衝突し、後から書いた方が上書きする

## 概要

控えのファイル名 `Series::stem` (crates/llm-gateway/src/cache/keepalive/store.rs) は session_id の英数字以外を `-` に潰すので、prefix まで同じ 2 つの会話 (例 `a b` と `a/b`) が同じファイルになり、後から書いた方が上書きする。stem のコメントは「衝突しないよう prefix を付ける」と書くが、会話どうしの衝突は防げていない。

読み出し側 (DR-0042 の `Store::load_meta_of`) は中の session_id の完全一致で取り違えを弾くので別の会話の状態を答えることは無いが、上書きされた側の控えは失われ、その会話の keepalive も止まる。

## 背景

Claude Code の session_id は UUID なので実運用で当たる確率は低い。DR-0027 決定 3 (置き場とファイル名) の範囲。発見: DR-0042 第 1 段の実装中 (2026-11-06)。

直すなら次のどちらか。

- stem にハッシュを足す (例: 潰した id + 短いハッシュ)
- 潰さずに percent-encode する

## 受け入れ条件

- [ ] 潰した結果が同じになる 2 つの session_id (例 `a b` と `a/b`) が別のファイルになる
- [ ] 上記を固定するテストがある
