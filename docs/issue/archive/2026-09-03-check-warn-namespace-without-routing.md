---
title: check/起動時に routing/alias が空の namespace を warning する
status: resolved
category: request
created: 2026-09-03T20:05:58+09:00
last_read:
open_entered: 2026-09-03T20:05:58+09:00
wip_entered: 2026-09-04T15:32:18+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-04T15:52:00+09:00
discard_reason:
pending_reason:
close_reason: ["done:実装済み (change sxswmvsyyxno)。Config::namespaces_without_routing / namespaces_without_aliases を追加し、check は namespace 要約行 (routing/aliases/cache 件数) + warning、serve 起動時は tracing::warn。事故状態を再現した config で warning が出ることを実機確認済み。受け入れ条件 4 件すべて充足"]
blocked_by:
origin: 自リポ TODO
---

# check/起動時に routing/alias が空の namespace を warning する

## 概要

`llm-gateway check` と起動時に、以下を warning する:

- routing 規則が 1 つも無い namespace
- aliases が空の namespace

あわせて check の出力に namespace ごとの要約 (routing 規則数 / alias 数 / cache 規則数) を 1 行ずつ出す。目視で異常な namespace に気づけるようにする。

## 背景

2026-09-03 実事故: config の手編集ミスで `[ns.personal]` の cache/filter/routing/aliases が丸ごと消えたが、有効な TOML だったため `check` は no problems を返した。約 4 時間、personal namespace のリクエストが既定順 (claude-emrd が先頭) で流れ、alias と cache 戦略も不適用のまま運用されてしまった。

routing 無しの namespace 定義自体は合法な config だが、実運用ではまず意図しない状態なので warning が妥当。

## 受け入れ条件

- [x] routing 規則 0 件の namespace で `check` が warning を出す
- [x] aliases 0 件の namespace で `check` が warning を出す
- [x] 起動時 (サーバ起動ログ) でも同様の warning が出る
- [x] `check` の出力に namespace ごとの要約行 (routing 規則数 / alias 数 / cache 規則数) が追加される
