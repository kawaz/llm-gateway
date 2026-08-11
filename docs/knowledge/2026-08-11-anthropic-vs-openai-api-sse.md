# Anthropic Messages API と OpenAI Responses API の比較 (SSE・オプション体系)

llm-gateway の codex ネイティブ対応 (DR-0014 P3) で両者の変換層を実装した際の知見。
変換の実装は `crates/llm-gateway/src/preset/openai/{request,response}.rs`、
OpenAI 側の一次調査は `docs/findings/2026-08-04-openai-codex-native-spec.md`。

## 1. リクエスト側の全容

| | Anthropic Messages API | OpenAI Responses API |
|---|---|---|
| endpoint | `POST /v1/messages` | `POST /v1/responses` (codex サブスク経路は `chatgpt.com/backend-api/codex/responses`) |
| 認証 header | `x-api-key: sk-...` (OAuth なら `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`) | `Authorization: Bearer` (codex 経路は加えて `ChatGPT-Account-Id` 必須) |
| version | `anthropic-version: 2023-06-01` (header 必須) | なし (URL/フィールドで吸収) |
| beta 機能 | `anthropic-beta: <flag>` header に列挙 | header でなく body パラメータや tool の型名で表現 |
| 会話状態 | 完全ステートレス。毎回 `messages[]` に全履歴 | `previous_response_id` でサーバ側に会話状態を持てる (`store=false` で無効化。gateway はこれ) |
| system prompt | `system` (トップレベル、text block 配列) | `instructions` (文字列) |
| 出力上限 | `max_tokens` 必須 | `max_output_tokens` 任意 (codex backend は非対応で drop) |
| 思考 | `thinking: {type: "adaptive"}` + `output_config.effort` (low〜max) | `reasoning: {effort: "low/medium/high/xhigh"}` |
| tools | `tools[] {name, description, input_schema}` + `tool_choice` | `tools[] {type:"function", name, parameters}` + `tool_choice`。組み込み tool も同じ配列 |
| キャッシュ | `cache_control: {type:"ephemeral"}` を block に明示 (prefix 一致、breakpoint 最大 4) | 自動 (明示制御なし。`prompt_cache_key` 程度) |

**オプション置き場の思想差**: Anthropic は「プロトコル制御は header (version/beta)、
内容は body」と分離。OpenAI は原則ぜんぶ body で header は認証だけ。
gateway の変換は body→body の写像 + 認証 header の付替え。

## 2. SSE イベント体系 (最も構造的な差)

**Anthropic**: 「message の中に index 付き content block が並ぶ」フラットな箱モデル。
イベントは 6 種のみ:

```
message_start                    ← usage の初期値 (input) もここ
content_block_start (index=0)    ← text / tool_use / thinking の開始
content_block_delta (index=0)    ← text_delta / input_json_delta / thinking_delta
content_block_stop  (index=0)
message_delta                    ← stop_reason と最終 usage (output)
message_stop
```

**OpenAI Responses**: 「response の中に output item (message / function_call /
reasoning...) が並び、item の中に content part が並ぶ」2 段ネストの item モデル。
イベント種は数十種:

```
response.created
response.output_item.added       ← item 単位 (message / function_call / reasoning)
response.content_part.added      ← item 内の part
response.output_text.delta       ← テキスト増分
response.function_call_arguments.delta
response.output_item.done
response.completed               ← usage はここに一括
(+ response.failed / error / reasoning 系イベント群)
```

主な意味論差:

- **粒度**: Anthropic は start/delta/stop の 1 パターンを全種に使い回す。
  OpenAI は種類ごとに専用イベント名が生える
- **usage**: Anthropic は message_start (input) + message_delta (output) に分割。
  OpenAI は `response.completed` に一括 (`output_tokens_details.reasoning_tokens`、
  `input_tokens_details.cached_tokens` の内訳付き)
- **終了理由**: Anthropic は `stop_reason` (end_turn/tool_use/max_tokens/refusal...)。
  OpenAI は completed の status + `incomplete_details`
- **tool call**: Anthropic は `tool_use` block (id/name/input)。OpenAI は
  `function_call` item (call_id/name/arguments)。id 体系が item id と call_id の 2 本

gateway の `preset/openai/response.rs` は item モデル → block モデルの状態機械。
対応の骨子: `output_item.added(message)` → `message_start`+`content_block_start`、
`output_text.delta` → `text_delta`、`function_call_arguments.delta` → `input_json_delta`、
`response.completed` → `message_delta` (stop_reason 写像) + `message_stop`。

## 3. レート制限・枠の観測面

| | Anthropic | OpenAI (codex サブスク) |
|---|---|---|
| 応答 header | `anthropic-ratelimit-unified-*` (5h/7d 使用率・reset) | `x-codex-primary-*` / `x-codex-secondary-*` / `x-codex-credits-*` (独自体系。標準の `x-ratelimit-*` ではない) |
| 429 の中身 | `retry-after` header + error JSON | `resets_in_seconds` / `resets_at` が **body** に入る (`Retry-After` 常在は未確認) |
| 枠照会 API | `GET /api/oauth/usage` (undocumented) | `GET /backend-api/wham/usage` |

この非対称が「拒否シグナルの読み方は provider の Metering の責務」という
DR-0014 の設計判断の根拠 (Anthropic は header で足りるが OpenAI は 429 body まで読む)。
