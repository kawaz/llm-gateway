---
title: 経路試行前に確定するクライアント向け 4xx がログに残らない
status: resolved
category: request
created: 2026-07-30T10:31:38+09:00
last_read:
open_entered: 2026-07-30T10:31:38+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-07-30T21:01:55+09:00
discard_reason:
pending_reason:
close_reason: ["done:v0.5.0 commit lnmunslpvktv で解決。入口で断る失敗を refused() funnel に集約し、4xx は warn / 5xx は error で 1 行 (ns/status/理由、messages 経路は req= span 付き)。UnknownModel 404 と ns token 401 のログ追跡テスト追加 (受け入れ条件充足)。既知の軽微な残り: /v1/models 経由の 404/401 は request_span 外のため req= が付かない (既存構造由来、実害が出たら別途)。"]
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
