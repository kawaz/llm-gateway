---
title: credential×モデル毎のデイリー使用量集計を蓄積・閲覧できるようにする
status: open
category: request
created: 2026-07-30T22:03:30+09:00
last_read:
open_entered: 2026-07-30T22:03:30+09:00
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

# credential×モデル毎のデイリー使用量集計を蓄積・閲覧できるようにする

## 概要

credential (アカウント) 毎 × モデル毎のトークン使用量 (input / output /
cache_creation / cache_read など) の**デイリー集計を蓄積**し、見られる画面を
用意する。

## 背景

kawaz 裁定 (2026-07-30):

- ccusage は単一 Claude アカウント分しか集計できず、複数アカウント運用では
  合算できない
- ccusage は叩くたびに全量を再集計するので遅い。一度集計した日は確定なので、
  以降は最新差分だけ追加していけば良い
- gateway は全アカウントの中継点なので、応答の usage を観測してデイリー
  集計するのに最適な位置にいる

### 裁定済みの設計方針

- **リアルタイム観測 (現行 `/llm-gateway/usage` の rate limit 表示) は現状
  維持・オンメモリのまま**。リクエスト毎の都度保存はしない (無駄 + 他経路で
  claude を使う分とはどのみちズレる)
- デイリー集計は蓄積 (永続化) する。確定した日は再集計せず、当日分だけ
  追記更新
- 見る画面: エンドポイント + CLI 表示など、どこかで見られれば良い

### 設計論点 (未確定、実装前に詰める)

- usage の取得点: SSE ストリーム応答の usage は message_start /
  message_delta イベント内にある。relay が body を透過している経路に tap を
  入れる必要がある (非 SSE は body JSON の usage)
- 蓄積形式: 例) `~/.local/state/llm-gateway/stats/YYYY-MM-DD.json`
  (credential×model→counters)。書き込みタイミング (定期 flush? 日跨ぎ?
  graceful shutdown?)
- 複数プロセス (8401/8402) が同じ stats を書く場合の排他 (DR-0010 の flock
  基盤が流用できる)
- 表示: `/llm-gateway/stats` エンドポイント + `llm-gateway stats` CLI

## 受け入れ条件

- [ ] gateway 経由の全モデル・全 credential のトークン使用量が日次で蓄積される
- [ ] 過去日の集計は再計算なしで即座に閲覧できる
- [ ] プロセス再起動を跨いで当日分が失われない (許容できる粒度で)
- [ ] CLI またはエンドポイントで credential×model×日のマトリクスが見られる
