---
title: 経路試行前に確定するクライアント向け 4xx がログに残らない
status: open
category: request
created: 2026-07-30T10:31:38+09:00
last_read:
open_entered: 2026-07-30T10:31:38+09:00
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

# 経路試行前に確定するクライアント向け 4xx がログに残らない

## 概要

gateway がクライアントへ 4xx を返すケースのうち、upstream への経路試行より前に確定するもの
(UnknownModel の 404、ns token 不一致の 401 等) は tracing ログに一切残らない。
upstream 転送を伴う失敗は経路切替・全滅時に warn ログが出るようになっているが、
gateway 自身が入口で断るケースが盲点として残っている。

## 背景

2026-07-30 00:04 頃、業務面セッションの claude-opus-4-8 (ns の filter で除外中のモデル) への
リクエストが 404 になったが、ログから原因を追跡できず、curl での再現によって原因を特定した。
入口 4xx にログが無いため、同種の事象は今後も再現手順を踏まないと原因特定できない。

## 受け入れ条件

- [ ] UnknownModel による 404 応答時、ns / model / status / reason が分かる 1 行ログ (warn or info) が出る
- [ ] ns token 不一致による 401 応答時も同様にログから追跡できる

## TODO

<!-- wip 時のみ -->
