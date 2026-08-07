# OpenAI native provider の実アカウント検証

## 前提

検証用設定に ChatGPT OAuth credential と OpenAI route を追加する。

```toml
[credentials.chatgpt-test]
type = "codex_oauth"

[routes.codex-test]
provider = "openai"
credential = "chatgpt-test"
models = ["gpt-5.3-codex"]

[[ns.default.routing]]
models = ["gpt-*"]
routes = ["codex-test"]
```

token や account ID はコマンド出力へ表示しない。認証情報ファイルの内容も表示しない。

## OAuth login

```bash
cargo run -p llm-gateway-cli -- login --type codex_oauth chatgpt-test --config path/to/config.toml
cargo run -p llm-gateway-cli -- check --config path/to/config.toml
```

確認すること:

- ブラウザが `https://auth.openai.com/oauth/authorize` を開く
- callback が `http://localhost:1455/auth/callback` へ戻る
- CLI とブラウザの両方が成功を表示する
- `check` が認証情報を読めると報告する

## gateway 起動と text response

```bash
cargo run -p llm-gateway-cli -- serve --config path/to/config.toml
```

別の端末から、設定した listen address へ送る。

```bash
curl -N http://127.0.0.1:11300/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  --data '{
    "model":"gpt-5.3-codex",
    "max_tokens":128,
    "stream":true,
    "messages":[{"role":"user","content":"Reply with exactly: ok"}]
  }'
```

確認すること:

- upstream への POST が `/backend-api/codex/responses` に届く
- client 応答が `message_start`、text block、`message_delta`、`message_stop` の順で流れる
- `max_tokens` を送った通常 request が 400 にならない
- log に token、refresh token、account ID が出ない

## tool use と複数 turn

1. function tool を付けた request を送り、`tool_use` block を受け取る。
2. 返った tool ID を `tool_result` で返す。
3. assistant 履歴に `thinking` または `redacted_thinking` block が含まれる request も送る。

確認すること:

- function arguments が `input_json_delta` として流れる
- tool result の次の turn が成功する
- thinking 履歴を含む次 turn が reject されない
- reasoning event が混ざっても text/tool block の index と stop event が壊れない

## parameter 対応

`temperature` と `top_p` を個別・同時に指定して送る。Codex backend が拒否した場合は、status と本文の error type だけを記録し、認証済み応答本文全体や header 値を貼らない。

`stop_sequences`、`top_k`、`metadata`、`service_tier` を指定した request は成功し、gateway log に drop の warning が出ることを確認する。

## quota と 429

```bash
cargo run -p llm-gateway-cli -- usage --probe --config path/to/config.toml
```

確認すること:

- `/backend-api/wham/usage` が Bearer と ChatGPT account ID で成功する
- primary / secondary window の使用率と reset が表示される
- quota 照会だけでは推論 request が発生しない

枠到達時または test account で 429 を観測できる場合は、次を記録する。

- `x-codex-primary-*` / `x-codex-secondary-*` header の存在する項目名
- `Retry-After` の有無
- body の `error.type`、`resets_at`、`resets_in_seconds` の有無と型
- gateway が reset まで route を外し、全 route が閉じた場合に 429 を返すこと

値そのものに account 情報が含まれる場合は記録せず、型と存在だけを残す。

## model discovery

起動時または refresh 時の request で次を確認する。

```text
GET /backend-api/codex/models?client_version=<llm-gateway version>
```

確認すること:

- 応答の配列名と model ID の field 名
- account / plan で catalog が変わるか
- discovery 失敗時は `routes.codex-test.models` の固定 fallback が公開される
- discovery 回復後は account catalog が固定 fallback を置き換える

## refresh rotation

access token の期限接近を待つか、検証用 credential の期限を現在時刻付近へ設定して gateway を起動する。

確認すること:

- refresh が 1 回だけ走る
- response に含まれた項目だけが更新される
- 新しい refresh token が返った場合は保存される
- refresh token が省略された場合は保存済みの値が維持される
- `refresh_token_expired`、`refresh_token_reused`、`refresh_token_invalidated` が再 login を促すエラーになる
