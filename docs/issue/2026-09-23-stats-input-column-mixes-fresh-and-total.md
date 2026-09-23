---
title: stats の input 列が Anthropic 行 (cache 除外) と OpenAI 行 (cache 込み総数) で意味が違う
status: wip
category: design
created: 2026-09-23T21:25:58+09:00
last_read:
open_entered: 2026-09-23T21:25:58+09:00
wip_entered: 2026-09-24T00:06:27+09:00
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

# stats の input 列が Anthropic 行 (cache 除外) と OpenAI 行 (cache 込み総数) で意味が違う

## 概要

`llm-gateway stats` の `input` 列 (`tokens.input`) は、Anthropic 系 upstream の行では cache を除いた入力、OpenAI 系 (gpt-*) の行では cache 分を含む総数として記録されている。USD は単価側 (`preset/pricing.rs` の `OPENAI_REFINEMENTS`) が内数を引くので正しい (検算一致) が、`--by origin` や合計行では性質の違う数の和になり、input の列で経路間を比べられない。

## 背景

2026-09-23 裏取り、docs/research/2026-09-23-harnessrouter-ui-observability.md §3.1 の指摘から。

- Anthropic: `preset/anthropic/metering.rs:365-375` が upstream の `input_tokens` (cache を含まない) をそのまま積む
- OpenAI: `preset/openai/metering.rs:270-318` の `read_usage` が input を常に総数に揃える (Responses は `input_tokens` そのまま、Messages 通訳時は引かれていた cache 分を足し戻す)。コメントの理由は「単価側が内数を引く相手を決められるよう、経路で意味を変えない」
- 公式仕様の根拠は `preset/pricing.rs:44-46, 77-83` のコメント (OpenAI の `input_tokens` は総数、`cached_tokens` はその内数)
- 実データ (2026-09-23): 合計 input 31,146,252 のうち約 3,000 万が gpt 行、Anthropic 行は約 7 万。gpt 行は cache_r < input、Anthropic 行は cache_r ≫ input

## 案 (裁定待ち)

- A. 表示時に揃える: 閲覧時に preset の内訳宣言 (refinements) で「cache を除いた入力」と「総入力」の両列に正規化。記録は不変。単価表に無いモデルは判定不能 (DR-0029 の「素性の無い過去ファイル」と同じ論点が表示側に残る)
- B. 積む時に Anthropic の意味へ揃える: `read_usage` の総数化をやめ、`OPENAI_REFINEMENTS` を外す。過去ファイルは総数のままなので「どちらの意味で書いたか」の印が要る (印なしは旧い意味として読む)。`read_usage` の設計理由と逆向き

## 裁定 (kawaz, 2026-09-24)

案 A の変種: 蓄積 (metering) は素直に (加工しない、upstream の生値をそのまま積む)。
gateway から外に出す時点 (stats の CLI 出力 / HTTP の stats 応答) で揃える。
view 側 (ccmsg 等の外部消費者) では対処しない — gateway の責務として stats 出力時に揃え切る。

理由: 蓄積時に特定解釈で加工すると、解釈の誤りや上流仕様の変更時の対応が面倒
(= 生値を保持しておけば後から解釈を直せる)。

揃える先: **cache を除いた入力** (Anthropic の意味)。cache 分は別に cache 列があるため、
input 列を「総数」にする理由がない。

## 受け入れ条件

- [ ] 裁定内容 (蓄積は生値のまま、stats 出力時に cache 除外の入力へ正規化) を DR-0029 か新 DR に記録
- [ ] `stats` の CLI 出力 / HTTP stats 応答で `--by origin` の input が Anthropic と OpenAI で同じ意味 (cache 除外) で並ぶ
- [ ] 過去の stats ファイル (蓄積側) は変更しない — 出力時の正規化のみで対応
- [ ] 過去の stats ファイルの USD が変わらない
