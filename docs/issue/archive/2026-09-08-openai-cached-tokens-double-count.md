---
title: OpenAI の usage で cached_tokens が input_tokens に含まれるのに二重課金している (gpt 系の USD が過大)
status: resolved
category: bug
created: 2026-09-08T12:36:37+09:00
last_read:
open_entered: 2026-09-08T12:36:37+09:00
wip_entered: 2026-09-08T12:37:30+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-08T12:51:28+09:00
discard_reason:
pending_reason:
close_reason: ["implemented","dr/DR-0011","dr/DR-0014"]
blocked_by:
origin: 自リポ TODO
---

# OpenAI の usage で cached_tokens が input_tokens に含まれるのに二重課金している (gpt 系の USD が過大)

## 概要

OpenAI (Responses / codex backend) の `usage.input_tokens` は `input_tokens_details.cached_tokens` を**含む**総数。gateway の `preset/openai/metering.rs::read_usage` は `input_tokens` を INPUT、`cached_tokens` を CACHE_READ に写し、pricing は両方を足すので cached 分が二重に課金される (INPUT 単価 + CACHE_READ 単価)。Anthropic は `input_tokens` がキャッシュ分を含まないので同じ扱いにしていた。

実例 (2026-09-08 stats): chatgpt-kawaz gpt-5.6-sol、input 46.8M / cache_r 14.8M → 14.8M × $5 = 約 $74 が過大 (表示 $244)。DR-0011 でコストは閲覧時計算なので、直せば過去日も正しくなる。

## 背景

併せて:

- `input_tokens_details.cache_write_tokens` (backend が返している、2026-09-08 実物で確認、値 0) を読んでいない。cache_w に記録し、単価は一次資料で確認 (5.6 世代から書き込み課金がある可能性: Azure docs「5.6 より前は課金なし」)。単価不明なら区分だけ記録して課金しない
- Messages に通訳する経路 (`response.rs`) が client に返す `input_tokens` も同じ意味 (総数) で返している。Anthropic 形式の `input_tokens` は非キャッシュ分なので、通訳では `input − cached` にするのが正しい (Claude Code 側の表示・集計にも効く)

## 方針 (案)

- 記録は upstream の数のまま (INPUT = 総数、CACHE_READ = cached) にし、pricing 側で OpenAI 行だけ「CACHE_READ は INPUT の内数」と宣言して親から引く (metering.rs の `Pricing.refines` の仕組み。今は REFINEMENTS が全行共通なので、行ごとの宣言に広げる)。過去の stats を書き換えずに済む
- または metering で INPUT = input − cached に正規化する (Anthropic と同じ意味に揃える)。過去の記録は総数のままなので閲覧時に不整合。DR-0011 / DR-0014 に照らして判断
- 通訳経路の `input_tokens` は `input − cached` に直す

## 受け入れ条件

- [ ] gpt 系の USD が (input − cached) × input 単価 + cached × read 単価 になる (test + stats の実値で確認)
- [ ] cache_write_tokens を記録 (単価は一次資料次第)
- [ ] 通訳経路の usage が Anthropic の意味に揃う
- [ ] pricing.rs の OpenAI 行のコメントと DR (0011 / 0014) を更新
