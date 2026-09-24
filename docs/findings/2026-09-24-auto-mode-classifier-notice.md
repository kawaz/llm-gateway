# auto mode の classifier 課金不適格通知は gateway による欄落としが原因ではない

- Date: 2026-09-24

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
