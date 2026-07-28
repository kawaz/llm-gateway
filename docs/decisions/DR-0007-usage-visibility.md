# DR-0007: 全 credential の利用量を一括で見えるようにする

- Status: Active
- Date: 2026-07-29

## Context

kawaz の要望:

> 各アカウントの 5h と 7d のコンテキスト利用料とリセット日時などを一括で確認できるものが
> 欲しいですね。今どのアカウントのクレジットがどれくらい残ってるのかとか確認できないのが不便。

調査 (2026-07-29 実測) で、必要な情報は upstream の応答から取れることが分かった。

### Anthropic (Claude OAuth)

`/v1/messages` の応答ヘッダに `anthropic-ratelimit-unified-*` が乗る (実測値):

| ヘッダ | 意味 | 実測例 |
|---|---|---|
| `unified-5h-utilization` | 5 時間ウィンドウの使用率 (0.0〜1.0) | 0.71 |
| `unified-5h-reset` | リセット時刻 (Unix 秒)。実時刻の 5 時間境界に丸まる | 05:00 JST |
| `unified-7d-utilization` / `-reset` | 7 日ウィンドウの同上 | 0.3 / 8/2 18:00 |
| `unified-{5h,7d}-status` | `allowed` など上限到達フラグ | allowed |
| `unified-overage-disabled-reason` | 従量課金フォールバックが塞がれた理由 | out_of_credits |

**公式ドキュメントに記載が無い** (documented なのは従量 API 向けの
`requests-*` / `tokens-*` のみ)。実測が唯一の根拠なので、予告なく変わる前提で扱う。
クレジット残の**数値**は取れない (overage の可否という 2 値だけ)。

副作用ゼロで usage だけ返す口は見つかっていない。ヘッダを得るには実リクエストが要る
(haiku + max_tokens=1 で input 8 / output 1 トークン)。

### Codex (ChatGPT OAuth)

2 経路ある (Codex CLI のソースで確認)。

- 応答ヘッダ `x-codex-{primary,secondary}-{used-percent,reset-at}` — Anthropic の 5h/7d に
  対応する 2 段窓。窓の長さ自体もサーバが返す (`window_minutes`)
- `GET https://chatgpt.com/backend-api/wham/usage` — **副作用なしの専用口**。
  クレジット残の数値 (`x-codex-credits-balance` 相当) やプラン種別まで取れる

### Bedrock

API key (推論実行権限) では使用量を取れない。使用量系は別の IAM アクションで、
Anthropic 側 Admin API の対象でもない (AWS Marketplace 課金)。**対象外と割り切り、
表示では「対象外 (AWS 課金)」と明示する**。

### Relay

転送先の gateway が同じ口を持てば再帰集約できる余地があるが、当面は対象外。

## Decision

**HTTP を正とし、CLI は薄いフォーマッタにする。**

- `GET /llm-gateway/usage` — credential ごとの利用状況を JSON で返す
  (名前・種別・5h/7d の使用率とリセット時刻・上限フラグ・overage 可否・**取得時刻**)
- `llm-gateway usage` — 上を叩いて人間向けに整形する CLI サブコマンド。
  server が起きていなければその旨を出す (CLI 単独ではスナップショットを持たない)

### 取得は「便乗」を基本にする

gateway は全リクエストを仲介しているので、**応答ヘッダを通りすがりに読んで
credential ID ごとの最新スナップショットを持つ**。追加の API コールはゼロ。

弱点は、しばらく使われていない credential の情報が古い/無いこと。そこで:

- スナップショットには必ず**取得時刻**を付け、表示側で古さが見えるようにする
- `?refresh=true` (CLI は `--refresh`) のときだけ**能動プローブ**する。
  Codex は副作用なしの `wham/usage`、Anthropic は haiku + max_tokens=1 の最小リクエスト
  (= usage の確認自体が usage を微量消費することを、出力に注記する)

既定を便乗のみにするのは、usage 計測が usage を勝手に消費する構図を避けるため。

### 認証は掛けない (healthz と同じ扱い)

出すのは使用率・リセット時刻・フラグだけで、credential の中身・token・
organization id は出さない。tailnet 内からしか到達できない前提で、
利用率の露出は許容する。問題が出たら締める。

## 関連

- [DR-0006](./DR-0006-namespace-routing.md) — `/llm-gateway/` 配下が gateway 自身の機能の置き場
- `docs/findings/` — unified ヘッダの実測記録
