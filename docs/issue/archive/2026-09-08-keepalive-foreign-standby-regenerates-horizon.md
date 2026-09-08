---
title: 二重 gateway の keepalive 引き継ぎ (Foreign → standby) が horizon を再生成する
status: resolved
category: bug
created: 2026-09-08T17:17:24+09:00
last_read:
open_entered: 2026-09-08T17:17:24+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-08T17:42:57+09:00
discard_reason:
pending_reason:
close_reason: ["dr/DR-0024","implemented","done:v0.43.6 で修正 (追補「合図の終わりは合言葉が持ち歩く」)、nonce を16バイト乱数+since_ms+horizon_end_msの32バイトに変更、Foreign受信側が絶対終了を引き継ぎ過去なら standby を作らない、テスト4本追加、just ci 814 passed、両機(11301/11302)に2026-09-08展開済み。二重gatewayでの実地確認は次回のkeepalive観測時にfindings 2026-09-08-keepalive-field-observationへ追記予定"]
blocked_by:
origin: 自リポ TODO
---

# 二重 gateway の keepalive 引き継ぎ (Foreign → standby) が horizon を再生成する

## 概要

二重 gateway 構成での keepalive 引き継ぎ (Foreign marker → standby 再作成) が
horizon を毎回リセットし、DR-0024 の「実リクエストだけが horizon を延ばす」が
プロセス間で破れている。

## 背景

2026-09-08 調査 (codex worker) で発見。業務面 ns の opus-5 / fable-5 セッション
10 本が、horizon 5.5h (≈6 本の ping で尽きるはず) を超えて **ちょうど 19 本**
(≈16.5h = 5.5h × 3 区間) の ping を受けていた。全応答は nonce のみ、marker 間に
実発話なし、間隔 55.1〜55.3 分。応答逸脱・単価未解決・count 計算ずれは証拠付きで
否定済み。

原因: `gateway.rs` の `Marker::Foreign` 分岐が
`keepalive.standby(series, bound, horizon_for(...), sent_at_ms)` を呼び、
`keepalive.rs` の `standby()` は `watch_of(series)` が `None`
(未観測、または自分の horizon が切れて畳んだ後) だと `Instant::now() + horizon`
を新起点として控えを作り直す。2 プロセス (例: 11301 と 11302) が交互に
「相手の marker を見る → 新しい standby を作る → 57 分後に引き継ぎが発火する →
相手が同じことをする」を繰り返し、horizon が系列共有の絶対上限として機能して
いない。

影響: opus の keepalive 「赤字」(findings
`2026-09-08-keepalive-field-observation`) の主因と推定。

## 修正方針 (統括裁定)

marker の nonce (現状: 32 バイト乱数の base64url 43 文字) を
「16 バイト乱数 + since_ms (8 バイト) + horizon_end_ms (8 バイト)」の
32 バイトに変え、文字数・形式は変えずに Foreign 受信側が絶対終了時刻と起点を
復号して引き継げるようにする。終了済みなら standby を作らない。peers 中継に
依存する方式より自己完結で、プロセス台数に依存しない。DR-0024 に追補する。

## 受け入れ条件

- [ ] marker nonce のエンコードに since_ms / horizon_end_ms を埋め込む変更が実装されている
- [ ] Foreign 受信側が終了済み horizon を検出して standby を再生成しないことが確認されている
- [ ] DR-0024 に本修正が追補されている
- [ ] 二重 gateway 構成での実地確認 (55 分間隔 ping が horizon 超過後に停止する) が取れている
