---
title: サブスク停止中の credential を「支払い待ちで利用不可」として扱う
status: resolved
category: request
created: 2026-09-09T16:21:18+09:00
last_read:
open_entered: 2026-09-09T16:21:18+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-09T17:01:03+09:00
discard_reason:
pending_reason:
close_reason: ["implemented","dr/DR-0009","done:v0.44.2 で実装。状態名は観測事実で org_not_allowed (kawaz 裁定: 同じ文言はサブスク停止以外でも出うる)、原因の推定は auth.hint に分離。403 + 締め出し文言を 1h cooldown (既存 RouteState に乗せ、明けに 1 本 probe で復帰)、model一覧/usage/quota の定期照会は cooldown 中スキップ、refresh は継続、login_path は付けない。MANUAL 更新、両 unit に 2026-09-09 展開済み"]
blocked_by:
origin: 自リポ TODO
---

# サブスク停止中の credential を「支払い待ちで利用不可」として扱う

## 概要

サブスク停止中の credential を、現状の「経路固有失敗として毎回そのまま切り替える」扱いから、
`subscription_inactive` という auth.status の新しい値として識別し、cooldown + 自動復帰 + 表示改善する。

## 背景

実測 2026-09-09: claude-zunsystem のサブスクを 9/9 に停止したところ、上流が全リクエストに
`403 permission_error: "OAuth authentication is currently not allowed for this organization."`
を返す (token の refresh は成功し続ける、quota API は 429)。

現状の gateway は 403 を経路固有失敗として毎回 zunsystem に当ててから切り替えるため、以下の問題がある:

1. 全リクエストに 1 往復分の遅延
2. 1h ごとの model 一覧 / usage 照会で warning
3. `usage` の表示が `(expired)` で誤解を招く (実態は「ログイン有効・支払い待ち」であって期限切れではない)

方針案:

- この type + 文言を `subscription_inactive` (auth.status の新値) として識別する
- (a) 候補から外して cooldown (1h 程度)、cooldown 明けに 1 本だけ試して 200 なら自動復帰
- (b) `usage` / `/llm-gateway/usage` の auth.status に載せて「ログイン有効・支払い待ち」と表示 (ccmsg の quota ページに伝わる)
- (c) refresh はそのまま続ける (支払い後に自動復帰)
- (d) config から外す必要は無い

関連: DR-0009 (denial と締め出し)、DR-0023 (auth.status)

## 受け入れ条件

- [ ] `403 permission_error: "OAuth authentication is currently not allowed for this organization."` を検出して `auth.status = subscription_inactive` に分類できる
- [ ] `subscription_inactive` な credential は候補選択から外れ、cooldown (目安 1h) 明けに 1 本だけ probe して復帰判定する
- [ ] `usage` / `/llm-gateway/usage` の表示が `(expired)` ではなく「ログイン有効・支払い待ち」等、実態に即した文言になる
- [ ] token refresh は `subscription_inactive` 中も継続される (支払い再開後に自動復帰できる)
