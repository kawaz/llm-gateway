---
title: request-dump-with-timeout (リクエスト本文の一時ダンプ)
status: wip
category: request
created: 2026-10-21T00:52:57+09:00
last_read:
open_entered: 2026-10-21T00:52:57+09:00
wip_entered: 2026-10-21T01:04:45+09:00
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

# request-dump-with-timeout (リクエスト本文の一時ダンプ)

## 概要

リクエスト本文の一時ダンプ。外部から時限で on/off でき、古いものは自動で片付ける。

DR-0038 を起草済み (docs/decisions/DR-0038-timed-request-dump.md、⬜ 未実装)。起草中に DR-0017 の tap が既に request_body / response_body を流せると分かり、新機能として作るか tap で済ませるかを docs/QUESTIONS.md の DP-Q1 で裁定待ちにした。

### 裁定 (kawaz 2026-10-21、DP-Q1 = a + 追加)

- **tap (DR-0017) を廃止して DR-0038 に一本化する** (DP-Q1a)。DR-0017 は `Superseded by DR-0038` で archive へ。`GET /llm-gateway/tap` と `tap::` モジュール、`TapObservation` は実装時に取り除き、本文を控える位置はダンプが引き継ぐ
- **request は gateway が受けた形** (ハーネスが送ろうとしたもの)、**response は upstream が返した生のバイト列**。gateway の書き換え (ttl 1h、thinking display、alias 解決) は events / stats 側で分かるので、ダンプには両端の生の姿だけを残す
- **`start` 1 回 = 1 系列、系列は `dump_id` (UUIDv7)**。置き場は `<[stats] dir>/dumps/<unit>/<ns>/<dump_id>/`。同じ ns に期間の重なる系列を複数作れる (1 本が両方に書かれるのは明示的に許容)。置き換えの規則・`replaced`・unit × ns で 1 つの制限・系列数の上限は持たない (`--for` 必須で自然に減る)。`start` の応答は `{dump_id, dump_dir, unit, ns, until, max_bytes, request_only}`、`stop <dump_id>|--all` (知らない id は `unknown_dump`、冪等で既に止まっていれば `active: false`)、`status` は走っている系列の一覧 (+ `bytes_written` / `requests`)
- **ファイルの形**: 系列のディレクトリに `index.jsonl` (1 req 1 行のメタ、ファイル名を含む) と、1 req ごとの `<ts>-<seq>.request.json` (受けた本文そのまま) と `<ts>-<seq>.response.sse` (upstream の生のバイト列。拡張子は応答の種類で変えない)。48h の片付けはディレクトリ単位

裁定は DR-0038 (決定 3・5・6・9・11・13・14・15) と MANUAL の `dump` 節に反映済み。実装に着手してよい。

## 背景

Claude Code 2.1.300 の jsonl 記録 (api-request / shape / blob) と gateway を実際に流れた body を突き合わせようとして、gateway 側に本文を保存する手段が無いことに当たった (2026-10-21)。DR-0024 の prompt cache 検証や「upstream が何を返したか」の調査でも毎回同じ所で困る。

### kawaz の要件 (2026-10-21)

- 常時ではなく**一時的に**有効化できるダンプ機能
- 溜まり事故を防ぐため、有効化は**外部から** on/off し、**必ず時限** (例: 「今から 10 分だけ」) で切れる
- 片付けを忘れるので、雑なタイミングで検査して **48 時間より古いダンプは勝手に消す**

### 統括の補強案

- 時間に加えて**容量の上限** (既定 200MB 程度) を持ち、超えたら止まる。並列 worker で 1 req 数百 KB が重なるため
- on/off は設定ファイルに書かず、`daemon reload` と同じ control socket 越しの操作にする: `llm-gateway dump start --ns <ns> --for 10m [--max-bytes N] [--request-only]` / `dump stop` / `dump status`。timer は unit が持ち、restart で消えてよい
- 片付けは unit 起動時と `dump start` 時に 48h 超を掃く (定期 timer を足さない)
- 置き場は `[stats] dir` 配下 `dumps/<unit>/<ns>/`、0600。Authorization ヘッダは落とす。`--ns` 必須で全 ns 一括はさせない (面の内容が別の面の作業で読まれる事故を減らす)
- request だけでなく response (SSE 生ストリーム) も取れる形にし、`--request-only` で絞れるようにする。cache 検証と upstream 応答の調査には response が要る

## 決める必要があること

- 1 ファイルの単位 (1 req = 1 ファイル / ns ごとの jsonl 追記)
- ダンプ中であることを stats や `/llm-gateway/events` で見せるか
- `daemon status` の出力に dump の残り時間を出すか

## 関連

- DR-0024 (prompt cache keepalive の検証に response が要る)
- docs/issue/2026-10-21-otel-observability-for-dev-environment.md (観測基盤。dump は調査用の一時手段で、常時観測は OTel 側)

## 受け入れ条件

- [ ] `dump start/stop/status` が control socket 越しに動き、時限で自動停止する
- [ ] 容量上限で停止し、48h 超のダンプが unit 起動時と `dump start` 時に掃除される
