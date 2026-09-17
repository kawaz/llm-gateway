---
title: plan command — routing 解決 + quota forecast を pull 型で出す
status: open
category: design
created: 2026-09-17T13:47:08+09:00
last_read:
open_entered: 2026-09-17T13:47:08+09:00
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

# plan command — routing 解決 + quota forecast を pull 型で出す

## 概要

`llm-gateway plan --ns <ns>` (+ `GET /llm-gateway/plan?ns=`) を足し、統括が委譲の直前に
「この ns でこの model を今投げたらどの credential に行き、その枠 (5h / 週) の残りと
リセット時刻、枠切れ時の次の行き先」を pull 型で確認できるようにする。

出力: ns ごと model 別 1 行 (now credential / 5h % / week % / week reset / next credential と
its week %)。JSON は同じ構造。

## 背景

kawaz 発案 (2026-09-17)、方向性は合意、着手指示は未。

狙い: 今は 429 で締め出されてから切り替わる構造を、切れる前に配分を変えられるように
する (例: sol の週枠が 88% でリセットまで 4 日、claude は 61% で 2 日 → 本気作業は
fable / opus xhigh に寄せる)。

設計方針:

1. gateway は事実だけを出す — routing の解決は実際の転送と同じ関数で dry-run (締め出し
   中の credential は飛ばす)、% は usage のスナップショット (`?refresh=true` で能動
   プローブ可)、next は now が枠切れした時の行き先
2. ペース推定 (今のペースでの使い切り予測) と助言文 (「贅沢に使え」) は入れない —
   方針の適用は読む側 (統括) の責務。要るなら後で足す
3. 受動注入 (hook) はしない — 統括が 6 時間に 1 回程度、委譲の判断時に叩く運用。
   worker-fleet skill 側に「本気の委譲の前に plan を見る」を足すのは
   claude-rules-personal への依頼 (このリポの責務外)

関連: DR-0007 (usage の便乗観測)、DR-0009 (締め出し)、routing 設定、
`docs/research/2026-09-15-claude-code-automatic-requests.md` (classifier が同じ枠を
食う点を注記)。

## 受け入れ条件

- [ ] `llm-gateway plan --ns <ns>` が ns×model 別 1 行で now/next credential・5h%・week%・
      week reset を出す
- [ ] `GET /llm-gateway/plan?ns=` が同じ内容を JSON で返す
- [ ] routing 解決は実際の転送経路と同じ関数を dry-run で使い、締め出し中の credential
      は候補から外れる
- [ ] `?refresh=true` で usage を能動プローブしてから応答する経路がある
- [ ] ペース推定・助言文は含まれない (設計方針 2 の逸脱がないことを確認)
