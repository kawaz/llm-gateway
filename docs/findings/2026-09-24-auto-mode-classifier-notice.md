# auto mode の classifier 課金不適格通知は gateway による欄落としが原因ではない

- Date: 2026-09-24

## 通知の原文 (kawaz の端末、2026-09-29 に再出現、Claude Code 2.1.284)

```text
We're changing auto mode to no longer charge for classifier requests in Claude Code.

However, this session isn't eligible because your requests go through llm-gateway.kawaz-mbp16-20211217.kawaz.jp, which isn't compatible with this update.

Nothing breaks: auto mode keeps working, and its classifier requests are billed as before.

To fix it and access the new version of auto mode, ask your gateway to implement: https://code.claude.com/docs/en/auto-mode-classifier-billing

y to continue · n to cancel
```

公式 docs (permission-modes の「Server-side classifier review」) による fallback の条件: 「a response completes with no review results, or the server answers that it doesn't review this session」。gateway 名は `ANTHROPIC_BASE_URL` から出しているだけで、gateway が原因だと判定した証拠ではない。`y` を押すとこの機械で 24 時間は再表示されない。

## 判明した事実

- Claude Code v2.1.278 以降、auto mode の classifier request を課金しなくなった判定は、request の `safeguards` 欄と応答 / SSE の `safeguard_results` 欄で往復するサーバ側の安全判定に基づく (出典: https://code.claude.com/docs/en/auto-mode-classifier-billing 、 https://code.claude.com/docs/en/llm-gateway-protocol#feature-pass-through)。gateway が未知の欄・ヘッダを落とす、応答の key を落とす、tool-use ID を書き換えると不適格になると明記されている。OAuth / プラットフォーム / credential が未展開でも不適格になり得るとも明記されている (gateway に問題が無くても発生し得る)
- Messages 経路の request は `crates/llm-gateway-server/src/lib.rs:697-707` で `serde_json::Value` として読み込み、`crates/llm-gateway/src/gateway.rs:461-472,1117-1129` で model と cache_control のみ加工、`preset/anthropic/wire.rs:80-94` で Value 全体を再 serialize しており、top-level の `safeguards` 欄は保持される
- DR-0016 の thinking 表示加工は `thinking` 欄のみに作用し、`safeguards` には触れない
- 応答は `preset/anthropic/wire.rs:117-145` で hop-by-hop 以外のヘッダと `bytes_stream` をそのまま返し、`egress.rs:236-244` / `exchange.rs:368-378` / `lib.rs:790-800` でも元の chunk をそのまま流している。`safeguard_results` や tool-use ID を書き換える変換は存在しない
- request ヘッダは `egress.rs:88-113` で認証ヘッダと接続依存ヘッダのみ除去し、`anthropic-version` / `anthropic-beta` / `anthropic-*` / `x-claude-code-*` は保持する
- `~/.local/state/llm-gateway/credentials/*.json` の `denied_beta` は全 credential で空 (2026-09-24 時点確認)
- 統括セッション (a3389ab9) の keepalive request 本文の控え 2 系列を確認したが、いずれも `safeguards` 欄が存在しない (うち片方の `anthropic-beta` には `dangerous-tool-use-2026-09-03` を含む)
- tap endpoint への接続は拒否され、応答側の `safeguard_results` は未観測のまま

## 実用的な示唆 / ベストプラクティス

- gateway が classifier 関連の欄・ヘッダ・beta を落としている証拠は無い。通知の原因は gateway 側の実装ではなく、この credential (subscription OAuth) で Claude Code が `safeguards` を送っていない、または上流 rollout が未対応である可能性が高い (未確認)
- 通知が邪魔な場合の回避策は `CLAUDE_CODE_AUTO_MODE_SERVER=0` (ただし classifier 課金免除は諦めることになる)
- gateway 側の変更は不要。上流の rollout 状況が進んだ時点で再確認する

## 検証の詳細

### コードパス追跡 (gpt-6-sol worker による調査)

| 対象 | 場所 | 結果 |
|---|---|---|
| request の `safeguards` 欄 | `crates/llm-gateway-server/src/lib.rs:697-707`, `crates/llm-gateway/src/gateway.rs:461-472,1117-1129`, `preset/anthropic/wire.rs:80-94` | Value 全体を再 serialize、model と cache_control のみ加工。`safeguards` 欄は保持される |
| 応答の `safeguard_results` 欄 | `preset/anthropic/wire.rs:117-145`, `egress.rs:236-244`, `exchange.rs:368-378`, `lib.rs:790-800` | ヘッダ・chunk をそのまま流す。書き換える変換は無い |
| request ヘッダの除去範囲 | `egress.rs:88-113` | 認証・接続依存ヘッダのみ除去。`anthropic-*` / `x-claude-code-*` は保持 |
| beta 学習による除外 | `denied_beta` (credential 保存ファイル) | 全 credential で空、除外は発生していない |

### 実機観測

| 項目 | 結果 |
|---|---|
| 統括セッション a3389ab9 の keepalive request 本文 (2 系列) | いずれも `safeguards` 欄なし |
| tap endpoint での応答観測 | 接続拒否のため `safeguard_results` は未観測 |

考察: gateway 側のコードパスに classifier 判定に必要な欄・ヘッダを落とす箇所は見当たらず、実機観測でも改変の痕跡は無い。原因切り分けとしては「gateway 側は問題なし」という結論まで裏取りできたが、Claude Code 側が `safeguards` をそもそも送っていない理由 (credential 種別 / rollout 未対応) までは未確認。API key 経路 (Bedrock 等) で同じ通知が出るかどうかも未検証。

## 2026-09-29 追調査

### request / response の観測

Claude Code 2.1.284 を `ANTHROPIC_BASE_URL=http://127.0.0.1:11301/ns-personal`、`CLAUDE_CODE_AUTO_MODE_SERVER=1`、`--permission-mode auto --model claude-opus-5 -p` で起動し、`Bash` に `pwd` を一度実行させた。unstable unit の loopback 専用 `/llm-gateway/tap?include=request_body,response_body&max_body=131072` で、対象の `claude-opus-5` request / response を取得した。Claude Code の応答は `/private/tmp`、API request は HTTP 200。tap の会話本文・token・credential は保存しない。

| 対象 | 観測結果 |
|---|---|
| request `safeguards` | `[{"type":"dangerous_tool_use", "classifier_context":{...}}]` が存在。`classifier_context.permission_mode` は `auto`。 |
| response SSE `message_delta.delta.safeguard_results` | `[{"type":"dangerous_tool_use", "status":{"type":"available", "tool_uses":{}}}]` が存在。これは tool call を含まない最終応答の結果で、直前の `Bash` tool call の判定内容は今回の抜粋では確認していない。 |
| 「この session を review しない」相当の応答 | 観測した最終応答では `unsupported` ではなく `available`。全 request については未確認。 |

### Claude Code 2.1.284 の実行ファイルから読めた判定

`/Users/kawaz/.local/bin/claude` の実体は `/Users/kawaz/.local/share/claude/versions/2.1.284`。同実行ファイルの組み込み JavaScript を文字列検索し、以下を確認した。

- `oee(e)` は third-party 経路で auto mode が有効な場合、`_St()&&!QJ()&&!hRe()&&!(Ie()==="firstParty"&&SRe())` を満たせば `arbiterWithLocalFallback` を返す。`_St()` は `CLAUDE_CODE_AUTO_MODE_SERVER` の明示値を優先し、指定が無ければ `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` が設定され、かつ `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST` が無いときに無効にする。関数名を持つ他の条件の詳細は今回未追跡。
- classifier 用 beta の定義は `auto_mode_classifier` = `auto-mode-classifier-2026-07-16`、`dangerous_tool_use` = `dangerous-tool-use-2026-09-03`。実 request ヘッダの両値の有無は tap がヘッダを収録しないため未確認。
- `fXn(e)` は `safeguard_results` が無い、`null`、同じ型が無い場合に `server_no_result`、`status.type==="unsupported"` の場合に `server_unsupported` と判定する。`status.type==="available"` は `tool_uses` を tool-use ID ごとに解釈する。`RRe(e)` は `server_unsupported`、`server_unavailable_disabled`、`server_no_result` を fallback 対象とする。
- 通知の表示条件 `Hen()` は feature flag `tengu_velvet_heron`、プラン種別 (`pro` / `max` / `team` 等は抑制)、gateway host の有無、直近 24 時間の了承時刻を読む。**`Hen()` と通知本文生成 `XUt(e)` は `safeguard_results` を読まない**。gateway host は設定 URL などから抽出する。したがって「gateway が incompatible」という通知文は、実 request が欠落した、あるいは gateway が判定欄を落としたという診断結果ではない。

### 結論と未確認事項

unstable 経路の実 request には `safeguards` があり、実 response には `available` の `safeguard_results` があった。少なくともこの request では gateway 側の改修事項を確認できない。通知は gateway host 等から機械的に構成され、実際の server-side 判定成功とは独立して出るため、通知の文言を gateway の非互換性の証拠にはできない。2026-09-24 の keepalive に `safeguards` が無かった理由と、2026-09-29 に通知が再出現した session のすべての tool-use 判定結果は未確認。実 request の `anthropic-beta` ヘッダの個々の値と `/status` の Auto mode server 行も未観測。
