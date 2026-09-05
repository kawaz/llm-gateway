---
title: セッション単位で keepalive を止める API (兄弟 gateway へ中継)
status: wip
category: task
created: 2026-09-05T19:50:52+09:00
last_read:
open_entered: 2026-09-05T19:50:52+09:00
wip_entered: 2026-09-05T19:51:49+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin:
---

# セッション単位で keepalive を止める API (兄弟 gateway へ中継)

## 概要

しばらく触らないと分かっているセッションの cache keepalive を、ccmsg の webui から止められるようにする。ccmsg daemon が gateway の HTTP API を叩く形で、セッションには何も流さない (止めたいセッションにコンテキストを生やさない)。

## 背景

kawaz 裁定 2026-09-05、ccmsg r275 での設計:

- API: `POST /llm-gateway/keepalive/pause` body `{session_id}`。session_id だけで足りる (UUID、keepalive は main 系列のみなので session 全体の意思として全 series を畳む)
- 受けた instance は該当 session の watch を畳み、停止を永続化 (nonce 表と同列、再起動を跨ぐ)
- **解除は実リクエスト (マーカー無し) が来た時点で自動**。明示的な解除 API は不要。マーカー付き応答は解除条件に数えない
- **多プロセス**: DR-0024 の「優劣なし」は保ち「存在を知らない」だけを緩める。config に兄弟 gateway の URL を **自分を含めた一覧** で持ち (両 config を listen 以外同一に保つため)、自分の listen と一致する項目は飛ばす。最初に受けた instance が兄弟の同じ API を叩く。2 次リクエストには中継しない印 (ヘッダ) を付けてループ防止
- 兄弟が落ちていた場合の取りこぼし: 起動時に兄弟へ停止一覧 (`GET /llm-gateway/keepalive/paused`) を問い合わせて自分の watch から畳む
- 状態の公開: request event / status に `keepalive_paused` を載せ、ccmsg webui が cache_expires_at のリングと同列で表示できるようにする
- ccmsg 側は Caddy の URL 1 本だけ知ればよい (HA は gateway の責務に閉じる)。gateway→ccmsg が複数 endpoint なのは役割の違い (gateway は透過 proxy、ccmsg はホストごとの管理者)
- 停止フラグの掃除: horizon (最大 24h) を超えたものは起動時に捨てる

## 受け入れ条件

- [ ] DR-0024 に追補 (兄弟の存在を知る / 停止 API / 解除条件)
- [ ] pause API と永続化、実リクエストでの自動解除
- [ ] 兄弟中継 (config の一覧、自分除外、ループ防止ヘッダ)、起動時の停止一覧同期
- [ ] event / status への keepalive_paused
- [ ] 11301/11302 の実機で: 停止 → 両 instance とも合図を出さない → 実リクエストで再開、を確認
- [ ] ccmsg 側へ webui ボタン + daemon からの POST を依頼 (claude-ccmsg リポに issue)
