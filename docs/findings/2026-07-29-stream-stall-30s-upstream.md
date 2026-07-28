# ストリーミング応答の中断は upstream の 30 秒無音が起点

- Date: 2026-07-29

## 判明した事実

- Claude Code に出る `Response stalled mid-stream` / `Connection closed mid-response` / `Server error mid-response` の原因は **upstream (Anthropic API) がストリーム中に約 30 秒無音になる**こと。gateway・前段 Caddy・ネットワーク経路・クライアント SDK 設定はすべて無罪と確定
- 根拠 1: gateway の観測点 (relay モジュール、2026-07-28 17:24 UTC 反映) が記録した失敗 3 件すべてに **30.0±0.1 秒の無音** (max_gap_ms = 30073 / 30008 / 30095) が入っている
- 根拠 2: upstream との接続がエラーで切れた記録 (「転送が途切れました」) は 0 件。接続は健全で、中身だけが止まる
- 根拠 3: 前段 Caddy の検証で、経路 (tailnet + TLS + Caddy) は 1.3MB ボディ × 30 回でも失敗 0 件・TTFB 0.082 秒。Caddy に読み取りタイムアウトも SSE バッファリングも無し
- 根拠 4: ライブラリ既定タイムアウトの調査 (reqwest 0.13.1 / hyper 1.11.0 / axum 0.8.9、Cargo.lock 固定版のソース確認) で、macOS で 30 秒に発火するものは存在しない
- 失敗の終わり方は 3 パターン: (a) クライアントが痺れを切らして切る → Caddy に `context canceled`、(b) 30 秒無音→再開→再度悪化→クライアント切断、(c) upstream 自身が SSE error イベントを送って正常クローズ (接続は切れない) → クライアントに「Server error mid-response」
- パターン (c) の実例: 2026-07-28 19:06:38.992 UTC、req=211、body 1.59MB、197.5 秒走行、54.5 秒地点に 30095ms の無音、最終的に正常クローズ。クライアント側エラー時刻とミリ秒単位で一致
- ボディサイズとの相関 (400KB 未満で中断 0 件) は**交絡**。大きいボディ = 長い文脈 = upstream の処理時間が長い、の代理指標であって原因ではない (Caddy 側の 300 件検証でサイズ由来の転送遅延は無いと確定)
- upstream の不安定さはストリーム開始前にも出ている: Caddy の access.log で 529 Overloaded ×32 / 503 ×32 / 429 ×52 (2026-07-28、/v1/messages 2227 件中)
- **30 秒の正体は Claude Code 側の byte stream watchdog** だった (後述の続報で訂正)。Claude Code v2.1.220 の実装で、無音がこの閾値を超えるとクライアントが接続を切り「Response stalled mid-stream」を表示する
- 閾値はコード既定 180 秒 (firstParty) だが、**リモート設定 `tengu_byte_stream_idle_timeout_ms` でサーバから動的に上書き**され、観測時点では 30000ms が配布されていたとみられる (観測 5 件が 30002〜30095ms に揃うことと整合)
- 「upstream が黙る」こと自体は upstream 由来のまま (トリガ)。**切るのはクライアント** (30 秒 watchdog)。当初の「upstream 内部に 30 秒タイマー」という解釈は誤りで、30 秒の規則性はクライアント側タイマーの signature だった
- リモート設定は任意のタイミングで変わるため、「2026-07-28 に急増し、それ以前 (cpa 経路時代) はほぼ出ていなかった」ことも経路と無関係に説明できる (7/28 の日別集計: 平時 1〜9 件/日 → 67 件)
- 対処: `CLAUDE_STREAM_IDLE_TIMEOUT_MS` (undocumented、バイナリで実在確認) を settings.json の env に設定すると、SSE idle が max(設定値, 5 分) になり、**byte watchdog へのリモート上書きも無効化される**。`API_TIMEOUT_MS` (documented、既定 10 分) も全体上限として併用。2026-07-29 に 3 面 (personal / emeradaco / emrd) へ `CLAUDE_STREAM_IDLE_TIMEOUT_MS=600000` / `API_TIMEOUT_MS=1200000` を適用済み。環境変数は起動時固定なので適用後に起動したセッションから有効

## 実用的な示唆 / ベストプラクティス

- gateway 側で対処できるものではない (SSE の中身には触れない方針、かつ発生源が upstream)
- Claude Code のリトライが緩和策として既に機能している。実害は「たまにエラーが見えて数秒待つ」に留まる
- 調査には gateway 側の観測点が必須だった。Caddy 側からは upstream 由来の異常が写らない (upstream がヘッダ送出後に死んでも Caddy は無記録で 200 を返すことを隔離環境で実証済み)
- 両側ログの突き合わせは応答バイト数 (gateway の `bytes` = Caddy の `size`) をキーにする

## 検証の詳細

### gateway 観測点 (relay モジュール)

| 項目 | 結果 |
|---|---|
| 反映期間 | 2026-07-28 17:24〜19:07 UTC |
| 完走 | 396+ |
| upstream エラー | 0 |
| クライアント切断 | 6 (うち max_gap 30 秒級 2 件、19.4 秒 1 件、残り 3 件は gap 1 秒未満でユーザの手動中断とみられる) |

観測点は `body_bytes` / `elapsed_ms` / `max_gap_ms` (最後のチャンクから終端までの無音も含む) / `max_gap_at_ms` (最長無音の開始時点)、終端 3 分類 (完走 / upstream エラー / クライアント切断) を記録する。

### 30 秒無音の実例

| req | body | 無音開始 | 無音長 |
|---|---|---|---|
| 165 | 326KB | 137.7 秒地点 | 30073ms |
| 279 | 1.45MB | 60.0 秒地点 | 30008ms |
| 211 | 1.59MB | 54.5 秒地点 | 30095ms |

考察: いずれも 30 秒付近に揃っており、gateway・クライアント側のタイムアウト値と一致しない (後述のライブラリ調査参照)。upstream 側のタイムアウト機構によるものと見るのが自然。

### Caddy 側検証 (別セッション実施)

| 検証項目 | 結果 |
|---|---|
| クライアント切断 3 方式 (SIGKILL / SIGINT / --max-time) | すべて `context canceled` |
| upstream プロセス kill | 無記録 |
| ヘッダ前の切断 | 502 + `EOF` のみ |
| 1.3MB ボディ × 30 回転送 | 失敗 0 件、TTFB 0.082 秒 |

考察: Caddy に読み取りタイムアウトや SSE バッファリングは無く、経路起因の中断は再現しない。

### ライブラリ既定タイムアウト調査

| ライブラリ | バージョン | 30 秒相当の既定タイムアウト |
|---|---|---|
| reqwest | 0.13.1 | 無し (TCP keepalive 15s/15s/3 回のみ、ACK が返れば切れない) |
| hyper | 1.11.0 | 無し |
| axum | 0.8.9 | 無し |

考察: Cargo.lock 固定版のソースを確認した範囲で、macOS 上で 30 秒に発火するものは無い。唯一実動しうる reqwest の TCP keepalive は本現象と無関係 (別途 `docs/issue/2026-07-29-upstream-tcp-keepalive-library-default.md` に記録済み)。

### 続報 (2026-07-29): 30 秒の正体

- Claude Code v2.1.220 のバイナリ (`~/.local/share/claude/versions/2.1.220`) を直接 grep して実装を確認した
- 該当コード (整形):
  - SSE イベント idle: `max(CLAUDE_STREAM_IDLE_TIMEOUT_MS || 0, 300000)` = 既定 5 分、5 分未満へは下げられない
  - byte watchdog: `CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS` が最優先 → 未設定かつ `CLAUDE_STREAM_IDLE_TIMEOUT_MS` も未設定なら **remote config `tengu_byte_stream_idle_timeout_ms`** (fallback: firstParty=180000ms) → clamp [10000ms, 1800000ms]
  - `CLAUDE_STREAM_IDLE_TIMEOUT_MS` を設定すると remote config の分岐に入らなくなる (= 30 秒上書きが外れる)
- 30 秒 gap の観測 5 件: 30073 / 30008 / 30095 / 30002 / 30010 ms。うち 2 件 (30095, 30010) は gap 後にストリームが再開しており、watchdog 発火とチャンク到達の競合で生き延びたケースと解釈できる
- 追加の実例 (2026-07-28 19:45:05Z): req=769, body 1.70MB, 306.7 秒走行, 266.8 秒地点で 30002ms → クライアント切断。クライアント側エラー時刻とミリ秒一致
- `API_FORCE_IDLE_TIMEOUT` (documented, v2.1.169+) は「5 分の idle タイムアウト」の有効/無効を制御する別系統で、今回の 30 秒とは別物
