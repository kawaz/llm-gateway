---
title: gateway の HTTP 経由で OAuth login を開始できるエンドポイント
status: resolved
category: request
created: 2026-08-27T15:35:23+09:00
last_read:
open_entered: 2026-08-27T15:35:23+09:00
wip_entered: 2026-08-28T00:43:45+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-08-31T00:58:50+09:00
discard_reason:
pending_reason:
close_reason: ["dr/DR-0023","implemented","done: v0.27.0 で実装・出荷、claude-zunsystem の実機再認証成功 (last_refresh 2026-08-30T15:51:50Z) でリモート完結を確認","done: v0.27.1 でダークテーマ対応追加"]
blocked_by:
origin: 自リポ TODO
---

# gateway の HTTP 経由で OAuth login を開始できるエンドポイント

## 概要

gateway の HTTP 経由で OAuth login を開始できるエンドポイントが欲しい。tailnet
リモートから再認証できる UX を実現する。

## 背景

現状 OAuth login の再認証はローカルでの操作が前提になっていると想定される。
tailnet 経由でリモートのマシンから gateway にアクセスしている場合でも、
ブラウザで login フローを開始・完了できる HTTP エンドポイントがあれば、
リモートからの再認証が可能になる。

## 受け入れ条件

- [ ] gateway に HTTP 経由で OAuth login を開始できるエンドポイントが実装されている
- [ ] tailnet リモートからアクセスした場合でも login フローが完結する

## TODO

<!-- wip 時のみ -->

- 設計は `docs/decisions/DR-0023-web-login-endpoint.md` として確定済み
- 実装は codex-sol-worker で進行中
