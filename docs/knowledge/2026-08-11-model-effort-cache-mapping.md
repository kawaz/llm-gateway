# model / effort の指定方法とキャッシュ条件の比較・相互変換

Anthropic Messages API と OpenAI Responses API における model・effort (思考深度) の
指定方法、prompt cache が効く条件、gateway (DR-0014 P3) での相互変換の実装。
関連: [2026-08-11-anthropic-vs-openai-api-sse.md](./2026-08-11-anthropic-vs-openai-api-sse.md)

## 1. model の指定

両者とも **リクエストごとに body の `model` フィールドで指定**。アカウント既定値の
ような仕組みはどちらにも無い (クライアント側の設定がデフォルトを担う)。

| | Anthropic | OpenAI |
|---|---|---|
| 指定 | body `model` (毎リクエスト必須) | body `model` (毎リクエスト必須) |
| ID 体系 | alias (`claude-opus-5`) と日付付き full ID (`claude-haiku-4-5-20251001`) の 2 層。alias 推奨 | 単一 ID (`gpt-5.6-sol` 等)。日付 suffix の慣習なし |
| 一覧 API | `GET /v1/models` (capabilities 付き) | `GET /v1/models`。codex サブスク経路は `GET /backend-api/codex/models` (アカウントによっては空配列 — 2026-08-11 実測) |

gateway では client が送った model 名を discovery の catalog / aliases で解決し、
route ごとの upstream 名に書き換えて送る (`router.rs` / `egress.rs`)。

## 2. effort (思考深度) の指定

**両者ともリクエストごと**。会話単位・セッション単位の設定は無い
(ステートレスなので毎回送る。Anthropic は途中で変えても会話は壊れない)。

| | Anthropic | OpenAI |
|---|---|---|
| パラメータ名 | `output_config: {effort: ...}` | `reasoning: {effort: ...}` |
| 値 | `low` / `medium` / `high` / `xhigh` / `max` (既定 high) | `low` / `medium` / `high` / `xhigh` (minimal を持つ系列もある) |
| 思考の on/off | 別パラメータ `thinking: {type: "adaptive"}` (4.7+ は adaptive のみ、Opus 5 以降は既定 on)。旧世代は `thinking: {type: "enabled", budget_tokens: N}` の数値指定 | `reasoning` を送らなければモデル既定。effort が実質 on/off と深度を兼ねる |
| 思考の可視性 | `thinking.display: "summarized"/"omitted"` | `include: ["reasoning.encrypted_content"]` 等 (codex 経路は暗号化 reasoning) |

構造の違い: Anthropic は「思考するか (thinking)」と「どれだけ頑張るか (effort)」が
直交した 2 パラメータ。OpenAI は `reasoning.effort` 1 本。さらに Anthropic の旧世代は
数値 (budget_tokens)、新世代は離散値という世代差もある。

## 3. prompt cache が効く条件

| | Anthropic | OpenAI |
|---|---|---|
| 制御 | **明示**。content block に `cache_control: {type: "ephemeral"}` (breakpoint 最大 4) | **自動**。明示制御なし (`prompt_cache_key` でルーティングのヒント程度) |
| 一致条件 | prefix のバイト一致 (`tools` → `system` → `messages` の順に render)。1 バイト違えば以降全滅 | prefix 一致 (自動判定)。長い共通 prefix があれば勝手に効く |
| 最小サイズ | モデル依存 512〜4096 token (短いと黙って効かない) | ~1024 token 相当 (公称) |
| TTL | 5 分 (書込 1.25 倍) / 1 時間 (書込 2 倍)。読出 0.1 倍 | 数分〜 (自動、課金割引は読出のみ) |
| model 変更 | キャッシュはモデル別 → 全滅 | 同じくモデル別 |
| effort 変更 | prefix (tools/system/messages) に含まれないので **無効化しない** | 同様に prompt 外のパラメータなので影響しない |
| tools 変更 | 先頭に render されるので全滅 (Opus 5+ は beta の mid-conversation tool changes で回避可) | prefix が変わる位置なら同様に切れる |

実務上の帰結: Anthropic は「安定部分を前に置き cache_control を打つ」設計作業が
必要 (llm-gateway 経由でも client の責務)。OpenAI は何もしなくて良い代わりに
制御もできない。gateway は本文素通しなので **どちらの cache 挙動も client の
リクエスト構造がそのまま決める** (gateway は cache_control を触らない)。

## 4. gateway での相互変換 (Anthropic 方言 → Responses API)

実装: `crates/llm-gateway/src/preset/openai/request.rs` の変換表。

- **model**: 変換不要 (routing/aliases の解決後の名前をそのまま送る)。
  例: client が `sol` → alias 解決で `gpt-5.6-sol` → codex route へ
- **effort/thinking → reasoning.effort**:
  - `thinking: {type: "adaptive"}` → `reasoning.effort = "high"`
  - `thinking: {type: "enabled", budget_tokens: N}` → N ≤ 2048 で `low`、
    N ≥ 16000 で `high`、中間は `medium` (閾値は変換表の定数)
  - `thinking: {type: "disabled"}` / 未指定 → reasoning を送らない
- **cache_control**: 送らない (落とす)。意味は性能ヒントで本文の semantics は
  不変のため。OpenAI 側は自動キャッシュに任せる
- 対応の無いパラメータの一般方針: 変換可能なら変換、劣化で済むなら drop + warn、
  semantics が黙って変わる構造的なもの (server tools 等) のみ reject
  (kawaz 裁定 2026-08-07)

つまり client (Claude Code) は Anthropic 方言だけ喋っていればよく、gpt 系モデル名を
指定した時だけ gateway が effort 込みで Responses API に写像する。
