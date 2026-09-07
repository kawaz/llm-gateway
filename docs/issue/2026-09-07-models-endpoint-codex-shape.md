---
title: codex CLI 向けに GET /{ns}/v1/models を ChatGPT backend の形で返す
status: wip
category: request
created: 2026-09-07T13:15:09+09:00
last_read:
open_entered: 2026-09-07T13:15:09+09:00
wip_entered: 2026-09-07T13:16:20+09:00
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

# codex CLI 向けに GET /{ns}/v1/models を ChatGPT backend の形で返す

## 概要

codex CLI (0.153) は起動時に `GET <base_url>/models` を叩き、ChatGPT backend の `/backend-api/codex/models` と同じ形 (`{"models": [...]}`、各要素に slug / display_name / metadata 等) を期待する。gateway は Anthropic 形式 (`{"object":"list","data":[{id,...}]}`) を返すため `failed to refresh available models: missing field models` の ERROR ログが出る。CLI は内蔵メタデータにフォールバックするので動作は正常 (2026-09-07 実機)。

## 背景

方針 (案):

- codex クライアントの判別は `originator` ヘッダ (`codex_exec` / `codex_cli_rs` 等) か `User-Agent` の `codex_` prefix。DR-0025 の RequestShape と同じく「受けた形」で分岐
- 返す中身: backend の `/backend-api/codex/models` をパススルーで取って返すのが忠実だが、kawaz のアカウントでは空配列を返す (config コメント 2026-08-11 実測) ので、gateway が知る codex 経路の `models` (config 宣言) を backend 形式に組み立てて返す案が現実的。metadata (context window 等) は codex の内蔵値に任せるため最小の欄 (slug / display_name) に留める
- 実物の期待形は codex CLI のソース (openai/codex の codex-rs/models-manager) で確認する

## 受け入れ条件

- [ ] codex CLI を gateway に向けて起動した時に models refresh の ERROR が出ない
- [ ] Claude Code / curl からの `GET /v1/models` は従来どおり Anthropic 形式
- [ ] DR-0025 に追記
