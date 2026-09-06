---
title: 応答完了の event (stop_reason 付き) を出して ccmsg がターン終了 = 入力待ちを判定できるようにする
status: open
category: design
created: 2026-09-06T21:38:05+09:00
last_read:
open_entered: 2026-09-06T21:38:05+09:00
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

# 応答完了の event (stop_reason 付き) を出して ccmsg がターン終了 = 入力待ちを判定できるようにする

## 概要

ccmsg がセッションの busy / idle (queue なしで入力できるか) を gateway の通知だけで判定したい (kawaz 2026-09-06)。既存の request event は upstream の応答ヘッダ到着時に出るが、ターンの終わりは分からない。応答本文の完了時に第二の event を出し、`stop_reason` を載せる。

## 背景

### 判定の根拠

Claude Code の 1 ターン = ユーザ入力 → (リクエスト → 応答 → ローカルでツール実行) の繰り返し → 最後にテキストで終わる。upstream の `message_delta` に `stop_reason` が載り、`end_turn` なら TUI は入力待ちに戻る (= queue なしで入力可)、`tool_use` ならクライアントがツールを実行して次のリクエストが来る。origin=sub は main のツール実行中に走るだけなので判定には使わない。クライアントが途中で切った (Esc) 場合も入力待ちに戻るので、切断も出す。

### 設計

- 新 event `type: "response"`。応答本文の転送が終わった時 (metering が usage を確定させる地点) または client 切断時に publish。webhook / SSE 両方
- 欄: `ts` (完了 or 切断の時刻、Unix ms)、`request_ts` (対応する request event の ts、対応付け用)、`session_id`、`prefix`、`ns`、`model`、`credential`、`origin`、`status`、`stop_reason` (upstream の値そのまま: end_turn / tool_use / max_tokens / stop_sequence / refusal…、取れなければ省略)、`aborted` (bool、client が本文完了前に切った)
- 時刻は Unix ms 整数、_iso 無し (DR-0012 の規約)
- count_tokens など転送でない経路には出さない。keepalive の合図の戻りにも出る (stop_reason end_turn) が、ccmsg は request 側の keepalive 欄で見分けられる
- DR-0012 に event 種と欄を追記、MANUAL の SSE 例に追加

## 受け入れ条件

- [ ] main の応答完了で type=response の event が stop_reason 付きで流れる (SSE で実機確認)
- [ ] client 切断で aborted: true の event が流れる (test)
- [ ] request_ts で request event と対応付けられる
- [ ] DR-0012 / MANUAL 更新、ccmsg へ通知
