---
title: codex 経路の discovery が gateway 自身の版を client_version に送り catalog が常に空
status: wip
category: bug
created: 2026-09-07T13:43:54+09:00
last_read: 2026-09-08T13:41:02+09:00
open_entered: 2026-09-07T13:43:54+09:00
wip_entered: 2026-09-08T13:42:58+09:00
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

# codex 経路の discovery が gateway 自身の版を client_version に送り catalog が常に空

## 概要

`discovery.rs` の `fetch()` が ChatGPT backend の `/backend-api/codex/models` を叩く時に `client_version` として gateway 自身の `CARGO_PKG_VERSION` (0.43.0) を送っている。backend は各モデルの `minimal_client_version` (gpt-6-astra は 0.153.0) と突き合わせて弾くため、**codex 経路の catalog は常に空**で、config の `models = [...]` 宣言へのフォールバックで動いていた。config のコメント「`/backend-api/codex/models` は空配列を返すアカウントがある (2026-08-11 実測)」は誤診。

実測 2026-09-07 (同一 credential): client_version=0.43.0 → 0 件、client_version=0.153.4 → 9 件 (gpt-6-astra / gpt-reserve / gpt-5.6-sol / terra / luna / gpt-5.5 / gpt-5.4-mini / gpt-5.3-codex-spark / codex-auto-review)。

## 背景

discovery による自動 catalog 更新が codex 経路で機能していないことに気付いた際の調査記録。

## 方針

- discovery の fetch が送る client_version を「codex CLI の最新版相当」にする。値の出所は要判断: (a) 定数 (リリースごとに更新、単価表と同じ運用)、(b) config で上書き可、(c) 直近に codex クライアントから受けた `User-Agent` の版を覚えて使う。まず (a) が単純
- 直すと catalog に `gpt-reserve` / `codex-auto-review` / `gpt-5.3-codex-spark` 等も載って `/v1/models` の公開一覧が変わる。filter (`[ns.*.filter] exclude`) で隠すか、config の `models` 宣言を優先する挙動を保つか、判断して DR に書く
- 両 config のコメント (routes.codex-* の「空配列を返すアカウントがある」) を訂正

## 受け入れ条件

- [ ] 起動時の discovery で codex 経路の catalog が非空になる (ログ `updated model catalog` の内訳で確認)
- [ ] `/ns-personal/v1/models` の一覧が意図どおり (余計なモデルが出るなら filter で隠す)
- [ ] config のコメント訂正、DR 追記
