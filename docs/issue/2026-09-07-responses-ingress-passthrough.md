---
title: codex CLI をクライアントにする: Responses API の受け口 + codex 上流へのパススルー
status: open
category: design
created: 2026-09-07T11:18:28+09:00
last_read:
open_entered: 2026-09-07T11:18:28+09:00
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

# codex CLI をクライアントにする: Responses API の受け口 + codex 上流へのパススルー

## 概要

codex CLI から gateway 経由で ChatGPT backend (codex) を使えるようにする (kawaz 2026-09-07)。第一段は **認証だけ gateway に任せるパススルー**: codex CLI が送る Responses API の本文をそのまま上流へ流し、Bearer を剥がして OAuth + chatgpt-account-id に差し替える。Claude 上流への変換 (Responses → Messages) は次段で別 issue。

## 背景

## 設計

- 受け口: `POST /{ns}/v1/responses` と `POST /v1/responses` (default ns)。codex CLI 側は `~/.codex/config.toml` の `model_providers.<name>` に `base_url = "https://…/ns-personal/v1"`、`env_key` (dummy)、`wire_api = "responses"` を書き、`model_provider = "<name>"` で向ける (書式は codex CLI 0.153 の実物で確認)
- 経路選定: 既存の `routes_for(ns, model)` と alias 解決をそのまま使う。Responses 形式を受けられるのは provider = openai の経路だけなので、選ばれた候補から openai 以外を除く (全滅なら 404 `no route can carry this request shape` 相当)。spend_down / 同格グループ / affinity も既存どおり
- 本文: **無変換**。model 欄だけ alias 解決後の名前に書き換える。`store` / `include` / `instructions` / `input` / `tools` は codex CLI が送ったまま
- ヘッダ: client の Authorization を剥がし、既存の `ChatGptBearer` (auth.rs) で OAuth + chatgpt-account-id を付ける。codex CLI が送る他のヘッダ (originator / User-Agent / session 系) は上流へ素通し
- 応答: 無変換でストリーム転送。metering は既存の OpenAI `UsageObserver` (response.completed の usage、x-codex-* の枠ヘッダ) がそのまま効く
- event: request / response event を出す。`origin` は Anthropic 本文の形で判定しているので Responses 形式には当たらない → `origin: "codex"` を新設 (main/sub/unknown と並ぶ値、DR-0012 に追記)。`session_id` / `prefix` は codex CLI が送るヘッダ・本文から取れるなら取る (無ければ省略)。cache 戦略 (keepalive) は Anthropic 固有なので適用しない
- count_tokens 相当は無し。`GET /{ns}/v1/models` は既存を流用可 (codex CLI が使うかは確認)
- MANUAL に codex CLI の設定例を追記

## 受け入れ条件

- [ ] codex CLI (0.153+) をカスタム provider で gateway に向け、`codex exec -m gpt-5.6-sol` / `-m astra` が通る (実機)
- [ ] usage / stats に codex CLI 経由の消費が載る、request / response event が origin=codex で流れる
- [ ] Anthropic 経路しか無いモデル (claude-*) を Responses 形式で要求すると 404 で明確なメッセージ
- [ ] 既存の Claude Code 経路 (Messages 形式) に影響が無い (test / 実機)
- [ ] DR (新規 DR で「Responses 受け口はパススルー、変換は次段」を記録)、MANUAL 更新
