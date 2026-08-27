---
title: gateway の HTTP 経由で OAuth login を開始できるエンドポイント
status: open
category: request
created: 2026-08-27T15:35:23+09:00
last_read:
open_entered: 2026-08-27T15:35:23+09:00
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
