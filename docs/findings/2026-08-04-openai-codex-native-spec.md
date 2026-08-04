# OpenAI Codex ネイティブ対応に必要な ChatGPT OAuth・Responses API 仕様

## 判明した事実

### ChatGPT OAuth

- Codex CLI の OAuth issuer は `https://auth.openai.com`、認可 endpoint は
  `https://auth.openai.com/oauth/authorize`、token endpoint は
  `https://auth.openai.com/oauth/token`。公開 client ID は
  `app_EMoamEEZ73f0CkXaXp7hrann`
  ([OpenAI Codex: client ID](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/auth/manager.rs#L1447-L1459),
  [認可 URL と token exchange](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/server.rs#L576-L612))
- interactive login は OAuth 2.0 Authorization Code + PKCE。`response_type=code`、
  `code_challenge_method=S256`、ランダムな `state` を使い、callback は通常
  `http://localhost:1455/auth/callback`。公式実装には登録済み fallback port 1457 もある
  ([OpenAI Codex: callback server](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/server.rs#L59-L63),
  [PKCE parameters](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/server.rs#L576-L612))
- 2026-08-04 時点の公式 Codex は scope に
  `openid profile email offline_access api.connectors.read api.connectors.invoke` を指定し、
  `id_token_add_organizations=true`、`codex_cli_simplified_flow=true`、`originator=<Codex の originator>`
  も付ける。参考実装の一部は古い最小 scope
  `openid profile email offline_access` のみで動かしている
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/server.rs#L584-L605),
  [wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/config.py#L5-L11))
- 公式実装の認可 URLには `audience` がない。wowyuarm 実装は
  `audience=https://api.openai.com/v1` を追加しているため、これは Codex CLI の現行必須項目とは
  確認できない
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/server.rs#L584-L605),
  [wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/auth.py#L264-L282))
- authorization code の交換は `application/x-www-form-urlencoded` で、body は
  `grant_type=authorization_code`、`code`、`redirect_uri`、`client_id`、`code_verifier`。
  成功応答として公式実装は `id_token`、`access_token`、`refresh_token` を必須として読む
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/server.rs#L809-L883))
- refresh は同じ token endpoint へ JSON で
  `client_id`、`grant_type=refresh_token`、`refresh_token` を送る。応答の
  `id_token`、`access_token`、`refresh_token` はすべて optional として扱い、返された値だけを
 置換する。refresh token はローテーションされうるため、新しい値が返ったら保存が必要
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/auth/manager.rs#L1308-L1375),
  [response shape](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/auth/manager.rs#L1433-L1445))
- refresh failure の既知 code は `refresh_token_expired`、`refresh_token_reused`、
  `refresh_token_invalidated`。再利用済み refresh token は恒久失敗として再ログイン対象になる
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/login/src/auth/manager.rs#L1378-L1405))
- account ID は JWT の `https://api.openai.com/auth.chatgpt_account_id` から得られる。
  実装例は access token または ID token の claim を署名検証なしで decode して取り出している
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/auth.py#L61-L95),
  [openai-oauth](https://github.com/EvanZhouDev/openai-oauth/blob/ec7dab2fcd8dab9da970a7a2b5dc34046c94905e/packages/core/src/runtime.ts#L209-L263))

### ChatGPT サブスクリプション用 Codex backend

- platform API の `https://api.openai.com/v1/responses` ではなく、ChatGPT 認証時の base URL は
  `https://chatgpt.com/backend-api/codex`、生成 endpoint は
  `POST https://chatgpt.com/backend-api/codex/responses`
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/model-provider-info/src/lib.rs#L38-L50),
  [wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/config.py#L16-L19))
- 認証に `Authorization: Bearer <access_token>` と `ChatGPT-Account-Id: <account_id>` を使う。
  body は `store=false`、`stream=true` が前提で、HTTP 応答は SSE。参考実装はさらに
  `Content-Type: application/json`、`Accept: text/event-stream` を付ける
  ([openai-oauth](https://github.com/EvanZhouDev/openai-oauth/blob/ec7dab2fcd8dab9da970a7a2b5dc34046c94905e/packages/core/src/runtime.ts#L811-L827),
  [wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/server.py#L272-L282),
  [request normalization](https://github.com/EvanZhouDev/openai-oauth/blob/ec7dab2fcd8dab9da970a7a2b5dc34046c94905e/packages/core/src/runtime.ts#L643-L664))
- `originator`、`User-Agent`、`x-codex-installation-id`、`x-client-request-id`、`session_id`、
  `x-codex-window-id`、`OpenAI-Beta` 等も Codex 系実装で送られるが、全リクエストの必須条件とは
  確認できない。最小の wowyuarm 実装は上記2認証ヘッダと `OpenAI-Beta: responses=experimental`
  で転送する一方、OpenAI 公式の現行実装や icebear 実装はより多くの client/session metadata を送る
  ([icebear/codex-proxy](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/src/proxy/codex-api.ts#L363-L414),
  [fingerprint headers](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/src/fingerprint/manager.ts#L91-L113),
  [wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/server.py#L272-L282))
- `max_output_tokens` は少なくとも2参考実装で upstream 送信前に削除される。その他、
  `temperature`、`top_p`、`stop`、`response_format`、`logprobs`、`seed`、`user` 等も
  Codex backend では非対応として drop されている
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/server.py#L51-L73),
  [openai-oauth](https://github.com/EvanZhouDev/openai-oauth/blob/ec7dab2fcd8dab9da970a7a2b5dc34046c94905e/packages/core/src/runtime.ts#L643-L664))

### Anthropic Messages から Responses API への変換

以下は wowyuarm/codex-proxy の実装から抽出した対応であり、OpenAI 公式仕様ではない
([request translator](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/translator.py#L188-L288))。

| Anthropic Messages | Responses API |
|---|---|
| `system` の string / text block 配列 | text を連結して `instructions` |
| user `text` | user item の `input_text` |
| user `image` (`base64` / `url`) | `input_image.image_url`。base64 は data URL 化 |
| assistant `text` | assistant message の `output_text` |
| assistant `tool_use` | `function_call` (`id`, `call_id`, `name`, JSON文字列 `arguments`) |
| user `tool_result` | `function_call_output` (`call_id`, string `output`) |
| `tools[].input_schema` | function tool の `parameters` |
| `tool_choice: auto` | `auto` |
| `tool_choice: any` | `required` |
| `tool_choice: tool` | `{type:"function", name}` |
| `tool_choice: none` | `none` |
| `thinking: adaptive` | `reasoning.effort=high` |
| `thinking: enabled`, budget `<=2048` | `reasoning.effort=low` |
| `thinking: enabled`, budget `>=16000` | `reasoning.effort=high` |
| `thinking: enabled`, 中間・budgetなし | `reasoning.effort=medium` |
| `thinking: disabled` | `reasoning` を付けない |

- 共通で `store=false`、`include=["reasoning.encrypted_content"]` を付ける。tools がある場合、
  同実装は `parallel_tool_calls=false` に固定する
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/translator.py#L269-L288))
- assistant history の Anthropic `thinking` block を `output_text` として再投入する処理は、
  thinking と通常 text の意味を区別しない参考実装固有の近似である
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/translator.py#L240-L267))
- Anthropic の `max_tokens`、`stop_sequences`、`temperature`、`top_p`、`top_k`、
  `metadata`、`service_tier`、cache control block、server tools、MCP blocks に相当する変換は、
  この参考実装にはない。特に `max_tokens` は Codex backend の `max_output_tokens` 非対応と衝突する

### Responses SSE から Anthropic Messages SSE への変換

参考実装の対応は以下
([Anthropic stream translator](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/translator.py#L628-L876))。

| Responses SSE | Anthropic SSE |
|---|---|
| 最初の output item / text | `message_start` |
| `response.output_item.added` の `message` | message 開始のみ |
| `response.output_item.added` の `function_call` | `content_block_start` (`tool_use`) |
| `response.output_text.delta` | text block の `content_block_delta.text_delta` |
| `response.content_part.delta` | 同上 |
| `response.function_call_arguments.delta` | `content_block_delta.input_json_delta` |
| `response.completed` | 現 block の `content_block_stop` → `message_delta` → `message_stop` |
| `error` / `response.failed` | Anthropic `error` event |

- `response.completed.response.usage.input_tokens` と `output_tokens` を Anthropic usage に写す。
  `input_tokens_details.cached_tokens` 等はこの実装では落ちる
- `response.status=incomplete` または incomplete reason が `max_tokens` / `max_output_tokens` の場合、
  Anthropic `stop_reason=max_tokens`。function call を出した場合は `tool_use`。通常は `end_turn`
- Responses の reasoning summary / encrypted reasoning item を Anthropic の `thinking_delta` /
  `signature_delta` に戻す実装はない

### 制限・usage・モデル一覧

- ChatGPT サブスク枠を照会する API は存在する。確認できた endpoint は
  `GET https://chatgpt.com/backend-api/wham/usage`。応答には `plan_type`、
  `rate_limit.primary_window`、`secondary_window`、`credits` があり、window は
  `used_percent`、`limit_window_seconds`、`reset_at` を持つ
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/usage.py#L22-L70),
  [endpoint](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/config.py#L16-L19))
- 別の参考実装は `/backend-api/wham/usage` のほか `/backend-api/codex/usage` も fallback として
  試す。どちらも `Authorization` と `ChatGPT-Account-Id` を使う
  ([icebear/codex-proxy](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/src/proxy/codex-usage.ts#L9-L58))
- Codex backend の rate-limit header は platform API の一般的な `x-ratelimit-*` ではなく、
  `x-codex-primary-used-percent`、`x-codex-primary-window-minutes`、
  `x-codex-primary-reset-at`、secondary の同系列、`x-codex-credits-*`、
  `x-codex-rate-limit-reached-type`。metered limit ごとに `x-<limit-id>-...` 系列もありうる
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/codex-api/src/rate_limits.rs#L22-L100),
  [credits / reached type](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/codex-api/src/rate_limits.rs#L178-L220))
- WebSocket 経路では `codex.rate_limits` event でも `plan_type`、primary / secondary window、
  credits を受け取る
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/codex-api/src/rate_limits.rs#L103-L167))
- quota 到達時の既知 error type は `usage_limit_reached`。参考実装は
  `usage_limit_reached`、`rate_limit_exceeded`、`rate_limit_reached` を HTTP 429 相当に分類する
  ([OpenAI Codex](https://github.com/openai/codex/blob/7431f10d0d4b4ecf5df08a853b41859013b17e45/codex-rs/codex-api/src/api_bridge.rs#L90-L120),
  [icebear/codex-proxy](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/src/proxy/ws-transport.ts#L43-L55))
- 429 body の参考実装上の reset 情報候補は `error.resets_in_seconds` または
  `error.resets_at`。`Retry-After` header が常に返ることはコードから確認できない
  ([icebear/codex-proxy](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/src/proxy/error-classification.ts#L32-L47))
- モデル一覧は固定リストだけではない。Codex backend の
  `GET /models?client_version=<version>`、すなわち production では
  `GET https://chatgpt.com/backend-api/codex/models?client_version=<version>` から catalog を取得する
  実装がある
  ([openai-oauth](https://github.com/EvanZhouDev/openai-oauth/blob/ec7dab2fcd8dab9da970a7a2b5dc34046c94905e/packages/core/src/models.ts#L168-L182),
  [icebear/codex-proxy](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/src/proxy/codex-models.ts#L11-L40))
- wowyuarm 実装の `/v1/models` は backend discovery ではなくコード内固定リストなので、
  モデル更新への追従には catalog endpoint を使う方が正確
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/codex_proxy/config.py#L31-L41))

### 利用規約上の位置づけ

- OpenAI 公式 Codex CLI 自身が同じ ChatGPT OAuth と backend を使用している一方、任意 client から
  relay することの許諾範囲はコードからは判断できない
- 参考実装はいずれも「非公式・OpenAI 非公認・変更や停止の可能性・自己責任・OpenAI の利用規約と
  rate limit / safeguard を遵守」と明記している
  ([wowyuarm/codex-proxy](https://github.com/wowyuarm/codex-proxy/blob/0930fe6243cf97c6221e8588a9135161eaf27b24/README.md#L192-L201),
  [openai-oauth](https://github.com/EvanZhouDev/openai-oauth/blob/ec7dab2fcd8dab9da970a7a2b5dc34046c94905e/README.md#L601-L609),
  [icebear/codex-proxy](https://github.com/icebear0828/codex-proxy/blob/4db59c48161809aab7f29695115a81f5e3ab10f0/README_EN.md#L764-L768))

## 実装への示唆

1. OAuth は公式 Codex の現行パラメータを正本にし、client ID、issuer、scope は定数ではなく
   将来差し替え可能な設定にする。callback は 1455 を第一候補とし、state を必ず検証する。
2. token record は `access_token`、`refresh_token`、`id_token`、JWT `exp` 由来の失効時刻、
   `chatgpt_account_id`、最終 refresh 時刻を保持する。refresh 応答は部分更新し、新 refresh token を
   取りこぼさない。
3. wire は platform OpenAI provider と分け、ChatGPT Codex provider の base URL、認証ヘッダ、
   `store=false` / 強制 SSE、非対応 parameter 除去を独立実装する。
4. request 変換は reference の近似をそのまま採用せず、対応不能項目を明示的に reject または
   warning 化する。特に `max_tokens`、thinking history、cache control、server tools、MCP、
   reasoning SSE の扱いを実装前に決める。
5. response translator は text と tool use だけでなく、Responses の reasoning item を調査してから
   Anthropic thinking block 対応を決める。usage は少なくとも input/output token を写し、cache token
   detail を保持できる設計にする。
6. quota は推論失敗を待たず `/wham/usage` を定期取得できる。429 時は
   `x-codex-*` header、body の reset、SSE / WebSocket event の全経路を解析し、credential cooldown に
   反映する。`Retry-After` の存在を前提にしない。
7. model discovery は固定リストを fallback にし、`/models?client_version=` の account 別応答を cache
   する。モデル catalog は account / plan / client version で変わる前提にする。
8. この経路は非公開 backend 依存として feature flag または明確な provider 種別に隔離し、endpoint
   変更時に platform API provider へ影響を波及させない。

## 検証の詳細 / 未確認事項

### 調査方法

- 2026-08-04 に以下の commit を clone し、README ではなく実装コードを優先して静的調査した。
  - OpenAI Codex: `7431f10d0d4b4ecf5df08a853b41859013b17e45`
  - icebear0828/codex-proxy: `4db59c48161809aab7f29695115a81f5e3ab10f0`
  - wowyuarm/codex-proxy: `0930fe6243cf97c6221e8588a9135161eaf27b24`
  - EvanZhouDev/openai-oauth: `ec7dab2fcd8dab9da970a7a2b5dc34046c94905e`
- secret を取得・表示せず、実アカウントでの OAuth login、token refresh、backend API 呼び出しは
  実施していない。

### 未確認事項

- access token / refresh token の実際の文字列形式。access token と ID token が JWT であることを
  前提に claim を読む実装は確認したが、実 token を観測していない
- access token の実際の有効期限。参考実装には `expires_in` を保存し、未指定時 3600 秒とするものが
  あるが、公式 Codex の authorization-code exchange は `expires_in` を読まず JWT `exp` を利用するため、
  常に1時間とは確定できない
- refresh token の最大寿命、通常 rotation の頻度、同時 refresh 時の厳密な再利用判定。公式コードから
  expired / reused / invalidated の区別は確認したが、期間は不明
- 現在の auth server が `audience=https://api.openai.com/v1` を受理する条件と、その有無による token claim
  の差。現行公式 Codex は送らない
- `originator`、Codex fingerprint header、cookie、Cloudflare 対策のうち、gateway の素朴な HTTP client で
  実際に必要な最小集合
- HTTP 429 の実 payload、`Retry-After` header の有無、全 `x-codex-*` header の実値。コード上の parser
  と参考実装の想定のみ確認した
- `/wham/usage` と `/codex/usage` の account / plan ごとの可用性、および polling が rate limit を持つか
- `/models?client_version=` の実 catalog schema、account / plan ごとの差、未知 client version を渡した時の
  挙動
- Responses SSE の reasoning summary / encrypted reasoning event の実イベント列と、Anthropic thinking block
  へ損失なく変換できるか
- `max_tokens` を送れない場合に、Anthropic caller の上限を gateway 側 cancellation で代替すべきか
- ChatGPT Plus / Pro subscription を汎用 relay として使うことが OpenAI Terms of Use のどの条項に該当するか。
  本文は法的判断をしておらず、実装前に現行規約と利用形態を別途確認する必要がある
