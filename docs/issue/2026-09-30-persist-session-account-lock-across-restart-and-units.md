---
title: session の開始 account と「跨いだ」印 (AccountLock) を永続化して restart と unit 間で共有する
status: open
category: design
created: 2026-09-30T18:19:57+09:00
last_read:
open_entered: 2026-09-30T18:19:57+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: 自リポ TODO (kawaz 着手裁定 2026-09-30)
---

# session の開始 account と「跨いだ」印 (AccountLock) を永続化して restart と unit 間で共有する

## 概要

session の開始 account と「跨いだ」印 (DR-0033 の `AccountLock`) を永続化して、restart と unit 間で共有する (kawaz 着手裁定 2026-09-30)。

現状は `Router.affinity` の `Binding` にメモリで持ち、寿命は affinity と同じ「最後に見てから 1 時間」、restart で消え、stable / unstable は別メモリ。

穴: 1 時間沈黙した session と、restart / unit 移動を跨いだ session は「跨いだこと」を忘れ、次の 2xx の account に再ロックして、他 account の thinking は API に黙って落とされる (text 化されない)。

## 方針

- Store 層 (共有ファイル + flock、DR-0010 の仕組み) に session ごとの `{ns, session, model, account, crossed, seen}` を置き、両 unit で共有する
- 寿命は affinity より長く (例 24h、`crossed` は session が生きている限り)
- issue `store-layer-for-replaceable-persistence` の 1 品目として、backend 差し替え可能な形にする
- affinity (route 名の優先) 自体は永続化しない (寿命 1h のメモリのままでよい)

## 設計で決めること

- 読み書きの頻度 (request ごとの read、2xx ごとの write)
- flock の競合
- reload / restart 直後の読み込み
- TTL 刈り込みの担当

## 背景

DR-0033 (thinking binding) の実装で `AccountLock` をメモリ保持にした結果の穴。

## 受け入れ条件

- [ ] DR を起草する
- [ ] restart を跨いでも「跨いだ」印が保たれる
- [ ] stable / unstable の unit 間で同じ session の lock 状態を共有する
- [ ] backend を差し替え可能な Store 層の形になっている

## TODO

<!-- wip 時のみ -->
