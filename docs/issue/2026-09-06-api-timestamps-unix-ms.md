---
title: usage / status / stats の JSON の時刻欄を Unix ms に統一し秒と _iso を全廃
status: wip
category: task
created: 2026-09-06T19:24:07+09:00
last_read:
open_entered: 2026-09-06T19:24:07+09:00
wip_entered: 2026-09-06T20:31:38+09:00
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

# usage / status / stats の JSON の時刻欄を Unix ms に統一し秒と _iso を全廃

## 概要

kawaz 裁定 (2026-09-06): 時刻の表現は Unix ms に統一し、秒精度と `_iso` 併記は全廃する (複数の表現が混在するのが害)。request / cache_keepalive / keepalive_paused の event は issue events-keepalive-until-and-pause-event で対応中。残りの JSON API を揃える。

## 背景

### 対象 (秒 + _iso の対を持つ欄)

- `GET /llm-gateway/usage` (quota.rs): `generated_at(_iso)`、credential の `observed_at(_iso)`、各窓の `reset(_iso)`、limits の `resets_at` (ISO 文字列)
- `GET /llm-gateway/status` (status.rs): `generated_at`、`observed_at`、`official_from` / `observed_from` 等 (実物で棚卸し)
- `GET /llm-gateway/stats` (stats.rs): `generated_at(_iso)` 等
- `GET /llm-gateway/keepalive/paused` は id の配列なので対象外。永続化ファイル (keepalive store の `fires_at` / `expires_at` / `horizon_end` / `paused_at`) は内部形式なので対象外でよいが、揃えるなら読み込み互換を保つ

### 方針

- 時刻は全て Unix ms の整数 1 欄。`_iso` は削除。upstream が秒で返す値 (Anthropic の reset 等) は gateway 側で ms に変換
- 長さ (`window_seconds` 等) は時刻ではないので対象外 (単位が名前に入っている)
- CLI (`llm-gateway usage` / `status` / `stats`) の人向け表示は CLI 側で整形 (JSON から iso を読まない)
- 消費者は ccmsg daemon (`llm-usage.ts`) と CLI のみ。出荷前に claude-ccmsg セッションへ新しい形を渡す
- DR-0007 / DR-0021 / DR-0011 等、欄を書いている DR は現在規則として書き直す

## 受け入れ条件

- [ ] usage / status / stats の JSON に秒精度の時刻欄と _iso 欄が残っていない (grep で確認)
- [ ] CLI の表示が従来どおり読める
- [ ] 関連 DR 更新、ccmsg 側へ通知
