---
title: CLI の help / error message で日本語と英語が混在している (DR-0008 途中適用)
status: wip
category: task
created: 2026-07-30T23:19:40+09:00
last_read: 2026-08-11T23:49:18+09:00
open_entered: 2026-07-30T23:19:40+09:00
wip_entered: 2026-08-11T23:50:08+09:00
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

# CLI の help / error message で日本語と英語が混在している (DR-0008 途中適用)

## 概要

`crates/llm-gateway-cli/src/main.rs` の CLI help (`USAGE` 定数) と error message
が、コマンドによって日本語と英語で混在している。DR-0008 (プログラムが出す文言は
英語にする) は「新規に書く文言から」適用する方針で、既存文言の一括置換はせず
残件を issue で追跡する運用のため、今回そのとおり起票する。

観測箇所 (`crates/llm-gateway-cli/src/main.rs`):

- `USAGE` 定数内: `serve` / `check` / `models` / `usage` / `login` の説明文と
  `usage のオプション` / `login のオプション` は日本語。`stats` の説明文
  (`list token usage per credential x model x day`, L26) と `stats options:`
  (L38-39) は英語
- `stats()` / `parse_stats_args()` / `take_days()` (L368-444) の error message
  はすべて英語 (`could not start the async runtime`, `--config needs a path`,
  `could not understand` 等)。一方で他コマンド (`login` 系など) は日本語の
  error message が残っている

## 背景

DR-0008 (`docs/decisions/DR-0008-user-facing-language.md`) より:

> 適用は「新規に書く文言から」。既存文言の一括置き換えは行わず、該当コードを
> 触ったついでに直す。残件は `docs/issue/` で追跡する。

`stats` サブコマンド (DR-0011 で新規追加) が DR-0008 適用後に書かれたため英語に
なっており、既存の `usage` / `login` 等はまだ日本語のまま。DR 自体は「意図的な
途中状態」を許容しているので bug ではなく、既存コマンドを触る機会に English 化
していくための追跡 task として起票する。

## 受け入れ条件

- [ ] `USAGE` 定数の全コマンド説明・オプション説明が英語に統一されている
- [ ] `main.rs` 内の error message / 表示メッセージが (ログを除き) 英語に統一
      されている (DR-0008 の対象外はログのみ)
- [ ] 該当コマンドを触る際についでに直す運用でよく、必ずしも一括変更でなくてよい
