---
title: Claude Code の /low-priority (anthropic-usage-limit: slow) の 429 slot_busy / 529 を DR-0009 の断りとして扱わず透過する
status: open
category: design
created: 2026-09-25T08:04:43+09:00
last_read:
open_entered: 2026-09-25T08:04:43+09:00
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

# Claude Code の /low-priority (anthropic-usage-limit: slow) の 429 slot_busy / 529 を DR-0009 の断りとして扱わず透過する

## 概要

Claude Code v2.1.282 の `/low-priority` (subscription の低優先度枠、feature gate で一部ユーザー) は request に `anthropic-usage-limit: slow` を付け、上流の応答ヘッダ `anthropic-ratelimit-unified-slow-{offer,status,retry-after,max-wait,budget-utilization}` を読んで、429 (`status=slot_busy`) や 529 を**クライアント側で待って再試行**する (既定約 20 秒 ± 30%、上限約 20 分、`retry-after` / `max-wait` に従う)。`weekly_limit` / `budget_exhausted` / `ineligible` / `off` は中断。

llm-gateway はヘッダ自体は request / 応答とも中継する (`egress.rs:89` の除去リストに無い、`preset/anthropic/wire.rs:68,118-146` で透過) が、**DR-0009 が 429 / 529 を「この経路に断られた」として即座に別 credential へ切り替える** (`gateway.rs:1963-2003`) ため、slow の「待てば通る 429」でも別 subscription の credential が通ってしまい意図しない枠を消費しうる。加えて 429 を受けた経路は `preset/anthropic/metering.rs:60-94` で最短 60 秒の model 別 cooldown が付き、次の request で全候補が cooldown なら router 自身の 429 (DR-0009:110-112) になって `slow-status` ヘッダが失われ、Claude Code の待機が働かない。

## 背景

DR-0009 の credential 切替は「上流に断られたら別経路を試す」という一般則だが、`slow` 枠は仕様上 429/529 を「待てば通常通り通る」応答として設計されており、断りの意味論が異なる。この非対称を無視すると、意図しない別 subscription の消費や、cooldown 全滅時のヘッダ消失による Claude Code 側リトライ機構の機能不全が起きる。

## 裁定が要る点

- `anthropic-usage-limit: slow` 付きの request は DR-0009 の例外にして、429 `slot_busy` / 529 を**断りとせずそのまま透過** (経路切替も cooldown もしない) するか
- それとも切替はするが、切替先も slow で送り、全滅時は slow ヘッダ付きの最後の応答を必ず透過する形にするか
- events に `slow` の素性 (offer / status) を載せるか (DR-0012)

## 未確認

- 非 streaming 経路の送信条件 `AA(URL)` が gateway の base URL でも真か (真でなければ gateway 経由では `/low-priority` 自体が効かない)
- 実 request の観測 (feature gate + 枠の壁が前提で `-p` では使えない。tap / events はヘッダを記録しないので `--debug` ログか別の観測口が要る)
- 課金 (表示は「uses your weekly limit」まで)

### 補足 (kawaz 2026-09-25)

X の投稿の読み: `/low-priority` は **5h 枠の壁に当たった後も (遅くてよければ) 作業を続けられる機能で、週枠を消費する**。binary の文言「Continue now at lower priority (uses your weekly limit)」「You've used this week's lower-priority allowance」と整合 (低優先度で使える量にも週の上限がある)。

含意: slow の request は 7d 枠を削るので、DR-0018 (spend_down) / DR-0019 (pace_cap) の判断とも噛み合わせが要る。裁定点に追加: slow 付き request を pace_cap の階段予算の対象にするか (本人が週枠の前借りを選んでいるので gateway が抑えるのは二重制限)、それとも通常どおり抑えるか。

## 受け入れ条件

- [ ] slow request の扱いを DR-0009 の追補 (または新 DR) として裁定
- [ ] 実装 + slow 付き 429 / 529 が透過されるテスト
- [ ] MANUAL に `/low-priority` との関係を記載
