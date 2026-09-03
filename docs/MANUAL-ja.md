# llm-gateway マニュアル

> [English](./MANUAL.md) | 日本語

llm-gateway が生やす HTTP 口と CLI コマンドのリファレンス。

口は 3 系統に分かれる。

| 系統 | パス | 用途 |
| --- | --- | --- |
| 転送 | `/v1/...`、`/ns-{name}/v1/...` | Anthropic Messages API をそのまま話す |
| 運用 | `/llm-gateway/...` | 死活・利用状況・統計・観測 |
| 再認証 | `/llm-gateway/login...` | ブラウザから OAuth をやり直す |

以下の例では gateway を `http://127.0.0.1:8402` で待ち受けている前提で書く。

## namespace

転送系のパスは先頭に `ns-` 付きの namespace を置ける。

- `/ns-personal/v1/messages` → namespace `personal`
- `/v1/messages` → 既定の namespace (`default`)

`ns-` 接頭辞を付けるのは、namespace 名と API のパス (`/v1/...`) を見分けるため
(DR-0006)。upstream へ渡すときに namespace の部分は取り除かれるので、upstream は
namespace を知らない。設定に無い namespace を指すと 404 が返り、本文に設定済みの
namespace 名が列挙される。

認証は namespace ごと。`[ns.<name>]` に `auth_token` を書いた namespace だけが
`Authorization` ヘッダを検査し、合わなければ 401 (`authentication_error`) を返す。
`auth_token` を書かない namespace は検査せずに通す — 手前 (tailnet / Caddy) で
境界を引く運用を前提にしているため。

## prompt cache 戦略 (`[[ns.<name>.cache]]`)

namespace ごとに、転送する本文の `cache_control` をどう扱うかを書ける
(DR-0024)。並びは `routing` と同じ「モデル glob + 先勝ち」で、照合するのは
短い名前を解決した後のモデル名。

```toml
[[ns.personal.cache]]
models = ["claude-fable-5-1*"]
main = "keepalive"
keepalive_horizon = "12h"

[[ns.personal.cache]]
models = ["*"]
main = "1h"
sub = "none"
```

| 欄 | 既定 | 意味 |
| --- | --- | --- |
| `models` | (必須) | 当てるモデル名のパターン。空だと設定エラー |
| `main` | `passthrough` | メインの会話からのリクエストに効く戦略 |
| `sub` | `passthrough` | サブエージェントからのリクエストに効く戦略 |
| `keepalive_horizon` | `8h` | `keepalive` で合図を出し続ける上限 |

戦略の語彙:

| 値 | 動作 |
| --- | --- |
| `passthrough` | 本文に触らない |
| `none` | `cache_control` を全て剥がす (使い捨ての 1 本向け) |
| `5m` | 全ブレークポイントの `ttl` 指定を落とす (= 既定の 5 分) |
| `1h` | 全ブレークポイントに `ttl: "1h"` を付ける |
| `keepalive` | `1h` と同じ本文にした上で、会話が止まったら合図を出して 1 往復を誘発し、cache を次の 1 時間へ繋ぐ。**`main` のみ**、`sub` に書くと設定エラー |

`keepalive_horizon` は 2 通りの書き方ができる:

| 書き方 | 例 | 意味 |
| --- | --- | --- |
| 時間 | `"12h"` | そのまま 12 時間。整数 + `h` のみ |
| 比率 | `0.3` | **分岐時間の 3 割**。0 より大きい実数 (1 以上も可) |

分岐時間 = (1 時間 write の単価 ÷ cache read の単価) × 55 分。合図を出し続ける
費用が cache の作り直し 1 回に追いつく点で、モデルの単価だけで決まる
(Fable 5.1 なら 80 回ぶん = 73.3 時間、Opus 5 なら 20 回ぶん = 18.3 時間)。
比率で書くと、単価の違うモデルに同じ判断基準を当てられる (`0.3` は Fable 5.1
で 22 時間、Opus 5 で 5.5 時間)。過去 7 日の実測では **0.2〜0.35** が目安
(`scripts/keepalive-horizon-sim.py`)。

単価表に無いモデルでは比率を時間に直せないので、既定の 8 時間に落ちる。
その組み合わせは起動時のログと `llm-gateway check` が名前を挙げる。

触るのは `cache_control` だけで、ブレークポイントの位置と数は変えない。
呼び出し元は 4 通りに分かれる。`metadata.user_id` に `parent_session_id` が
あればサブエージェント (`sub`)。無い場合は `system` 先頭ブロックの請求ヘッダを
見て、`cc_entrypoint` が `cli` 以外 (`sdk-cli` = `claude -p` 等) なら 1 回きりの
呼び出し (`oneshot`)、`cli` か請求ヘッダ無しならメイン (`main`)。`metadata` が
読めない相手 (`unknown`) はメイン扱い。**`sub` 側の戦略に乗るのは `sub` と
`oneshot`** — どちらも続きが来ないので、続きを当て込んだ扱いをしても報われない。

`keepalive` は合図を `webhook` 経由でしか届けられない。`webhook.base_url` を
書いていない設定では合図を出さない (= `1h` と同じ動作)。`llm-gateway check` が
その namespace を警告として挙げる。

### keepalive の運用

止まっている会話の見張りは `<stats の置き場>/keepalive/<待ち受け>.json` に残り、
再起動で読み戻される (`stats.dir` の既定は `~/.local/state/llm-gateway/stats`)。
待ち受けごとに別ファイルなので、同じ置き場を複数プロセスで共有してよい。

複数プロセスを LB の後ろで動かす場合、合図は**観測だけで 1 本に収束する**
(他プロセスの合図を見たら控えに回る)。振り分けは優先度型 (Caddy の
`lb_policy first`) か、`X-Claude-Code-Session-Id` ヘッダを鍵にした sticky を
推奨。round-robin でも収束するが、収束するまでの重複した合図が増える。

## 転送系

### `POST /{ns}/v1/messages`

Anthropic Messages API へそのまま中継する。応答は本文を溜めずに流すので、
`"stream": true` の SSE もそのまま通る。

- 認証: namespace の設定に従う
- リクエスト本文の上限: 64 MiB
- `model` は namespace の routing に従って経路が選ばれ、必要なら実モデル名へ
  解決してから upstream に渡す

namespace に `thinking_display` が設定されている場合、クライアントが `thinking` を
明示したリクエストに限り `thinking.display` を上書きする (DR-0016)。`thinking` の
無いリクエスト、`thinking.type = "disabled"`、`tool_choice` が `any` / `tool`、
末尾が assistant メッセージ (prefill) のリクエストは一切書き換えない。

```bash
curl -sS http://127.0.0.1:8402/ns-personal/v1/messages \
  -H 'authorization: Bearer <token>' \
  -H 'content-type: application/json' \
  -d '{"model":"opus","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}'
```

応答は upstream のものをそのまま返す (`{"type":"message","content":[...]}`)。
gateway 側で断る場合は Anthropic のエラー形式に揃えた JSON を返す。

```json
{"type": "error", "error": {"type": "invalid_request_error", "message": "..."}}
```

エラーの対応:

| 状況 | ステータス | `error.type` |
| --- | --- | --- |
| namespace のトークン不一致 | 401 | `authentication_error` |
| 本文が読めない / JSON でない | 400 | `invalid_request_error` |
| 未知の namespace | 404 | `invalid_request_error` |
| モデルがどの経路にも無い | 404 | `not_found_error` |
| 全経路が失敗 / upstream に届かない | 503 | `api_error` |

### `POST /{ns}/v1/messages/count_tokens`

`/v1/messages` と同じ中継。トークン数の見積もりを upstream に問い合わせる。

```bash
curl -sS http://127.0.0.1:8402/v1/messages/count_tokens \
  -H 'content-type: application/json' \
  -d '{"model":"opus","messages":[{"role":"user","content":"hi"}]}'
```

### `GET /{ns}/v1/models`

その namespace から使えるモデルの一覧。クライアントのモデル選択に出る。
見える内容は namespace の routing 設定によって変わる。

- 認証: namespace の設定に従う

```bash
curl -sS http://127.0.0.1:8402/ns-personal/v1/models
```

```json
{"object": "list", "data": [{"id": "claude-opus-5", "object": "model", "type": "model"}]}
```

## 運用系

`/llm-gateway/` の下にまとめてあるのは、upstream の API 名と衝突させないため
(DR-0006)。運用系はいずれも認証を持たない。境界は手前で引く。

### `GET /llm-gateway/healthz`

生きているかだけを返す。credential にも upstream にも触らない。前段の
ロードバランサが数秒ごとに叩く前提。

```bash
curl -sS http://127.0.0.1:8402/llm-gateway/healthz   # => ok
```

### `GET /llm-gateway/usage`

credential ごとの利用状況 (DR-0007)。出すのは使用率・リセット時刻・締め出しの
状態だけで、token も organization id も出さない。

| パラメータ | 既定 | 意味 |
| --- | --- | --- |
| `refresh` | なし | `true` / `1` のときだけ能動プローブに入る |

既定では転送に便乗して読んだ分しか出さない。**usage の確認自体が usage を消費する**
構図を避けるため。`?refresh=true` を付けると休んでいる credential へ最小の
リクエストを投げて読み直し、その消費量を `probe` に残す。

```bash
curl -sS 'http://127.0.0.1:8402/llm-gateway/usage?refresh=true'
```

```json
{
  "generated_at": 1785326400,
  "generated_at_iso": "2026-07-29T12:00:00Z",
  "probe": {"requests": 2, "model": "claude-haiku-4-5", "input_tokens": 18, "output_tokens": 1},
  "credentials": [
    {
      "name": "personal",
      "type": "claude_oauth",
      "support": "observed",
      "auth": {
        "status": "relogin_required",
        "reason": "log in again",
        "login_path": "/llm-gateway/login/personal/start",
        "observed_at": 1785326390,
        "observed_at_iso": "2026-07-29T11:59:50Z"
      },
      "snapshot": {
        "observed_at": 1785326390,
        "observed_at_iso": "2026-07-29T11:59:50Z",
        "5h": {"utilization": 0.71, "status": "allowed", "reset": 1785340800, "window_seconds": 18000},
        "7d": {"utilization": 0.34, "status": "allowed"}
      }
    }
  ]
}
```

`denials` は gateway が現在控えている締め出しと、その理由・範囲 (DR-0020)。
`limits` は枠照会 API から聞いた枠で、応答ヘッダ由来の `snapshot` とは別物として
持つ (同じ枠を指すとは限らない)。値が取れない欄は出力ごと省かれる。`claude_oauth` の
`auth.status` が `relogin_required` のときは、`auth.login_path` に Web login ページの
相対パスが入る。

### `GET /llm-gateway/status`

設定した upstream サービスの公式状態と実測状態 (DR-0021)。

| パラメータ | 既定 | 意味 |
| --- | --- | --- |
| `refresh` | なし | `true` / `1` のとき公式ソースを取り直してから返す |

```bash
curl -sS 'http://127.0.0.1:8402/llm-gateway/status?refresh=true'
```

```json
{
  "schema_version": 1,
  "generated_at": 1785326400,
  "overall": {"severity": "ok", "service_counts": {"ok": 2, "warning": 0, "critical": 0, "unknown": 0}},
  "services": [
    {
      "id": "anthropic",
      "name": "Anthropic",
      "severity": "ok",
      "routes": ["personal"],
      "official": {
        "state": "operational",
        "source": "anthropic",
        "source_url": "https://status.anthropic.com/",
        "observed_at": 1785326100,
        "stale": false,
        "components": [],
        "incidents": []
      },
      "observed": {"state": "ok", "observed_at": 1785326390, "last_success_at": 1785326390}
    }
  ]
}
```

`official` は公式ステータスページ由来、`observed` はこの gateway 自身が転送で
観測した成否。両方を並べるのは、公式が operational でも実際には通っていない
(あるいはその逆の) 場面を見分けるため。

### `GET /llm-gateway/stats`

使用量の日次集計 (DR-0011)。日 × credential × モデルのトークン数と、単価表が
ある分の USD 換算を返す。何を書いたかは残していない。

| パラメータ | 既定 | 意味 |
| --- | --- | --- |
| `days` | `7` | 直近 N 日に絞る。`0` で全期間 |

既定を 7 日にするのは、全期間を返すと日が経つほど応答が伸びるため。
数字として読めない値を渡すと 400 を返す。

```bash
curl -sS 'http://127.0.0.1:8402/llm-gateway/stats?days=3'
```

```json
{
  "generated_at": 1785326400,
  "generated_at_iso": "2026-07-29T12:00:00Z",
  "days": {
    "2026-07-29": {
      "credentials": {
        "personal": {
          "claude-opus-5": {
            "requests": 12,
            "input": 1200,
            "output": 340,
            "input.cache_read": 8800,
            "usd": 0.42
          }
        }
      },
      "total_usd": 0.42
    }
  },
  "total_usd": 1.13
}
```

`total_usd` は単価表にあるモデルの分だけを足す。1 行も出せなければ欄ごと省く
(出ている数字が全体の額に見えないようにするため)。

### `GET /llm-gateway/events`

転送のたびに起きたことを SSE で流し続ける (DR-0012)。届くのは**繋いだ後**に
起きた分だけで、過去には遡らない。見ている側が遅れて取りこぼしても gateway は
詰まらない (落として先へ進む)。20 秒ごとに keep-alive を送る。

`Access-Control-Allow-Origin: *` を付けているので、ブラウザから直接開いて
様子を見られる。本文もトークン数も流さない。

```bash
curl -sSN http://127.0.0.1:8402/llm-gateway/events
```

```
event: request
data: {"ts":1785326400,"ts_iso":"2026-07-29T12:00:00Z","session_id":"s-1","ns":"default","model":"claude-opus-5","credential":"personal","status":200,"prefix":"3f9a1c02","origin":"main","cache_ttl_secs":3600,"cache_expires_at":1785330000,"cache_expires_at_iso":"2026-07-29T13:00:00Z"}
```

`prefix` は system prompt の先頭ブロックのハッシュ (8 桁) で、同じ会話系列かを
見分ける印。取れなければ欄ごと出ない。`origin` はその 1 本を出した側
(`main` / `sub` / `oneshot` / `unknown`)。`cache_ttl_secs` は**この 1 本が残す
プレフィックスの寿命** (秒) で、効かせた戦略から決まり、本文に触らない場合は
送った `cache_control` を読む (`ttl:"1h"` があれば 3600、無ければ 300)。
`cache_expires_at` / `cache_expires_at_iso` はその時刻。ブレークポイントの
無い 1 本では 3 つとも欄ごと出ない。経路選定で外した経路がある場合は
`skipped` に credential と理由が並ぶ。合図の戻りだった 1 本には
`keepalive` (`applied` / `late` / `foreign`) が付く。

`keepalive` 戦略の namespace では、会話が止まったときに別種の 1 通が流れる
(DR-0024)。

```
event: cache_keepalive
data: {"type":"cache_keepalive","ts":1785326640,"ts_iso":"2026-07-29T12:04:00Z","session_id":"s-1","prefix":"3f9a1c02","nonce":"5Qv…","deadline":1785326670,"deadline_iso":"2026-07-29T12:04:30Z","marker":"[llm-gateway cache keepalive] token=`LLMGW-KEEPALIVE-5Qv…`; reply with a single line containing only that token, nothing before or after"}
```

受け取った側は `marker` をその会話 (`session_id`) へそのまま流し込む。
`nonce` は 32 バイトの乱数を base64url にした 43 文字で、`LLMGW-KEEPALIVE-` を
頭に付けたものが合言葉。返事はその 1 行だけになる。
戻ってきた 1 本の本文は普通のリクエストと同じ扱い (`keepalive` は常に 1 時間を
書く) で、`deadline` までに戻れば `applied`、過ぎていれば `late` として知らせに
出る。**この gateway が出していない合言葉**が戻ってきた場合は `foreign` —
同じ会話を見ている別プロセスの合図で、こちらは控えに回る (合図が 1 本に
収束する仕組み、DR-0024)。直前に通った経路が塞がっている間は合図そのものを出さない。同じ受け口
(`webhook`) にも同じ形で届く。

### `GET /llm-gateway/tap`

転送 1 件ごとの詳細を JSONL (1 行 1 JSON) で流す (DR-0017)。SSE ではないので、
そのままファイルに落として集計できる。

**直接 loopback から繋いだ接続だけが使える。** loopback 以外からの接続、および
`Forwarded` / `X-Forwarded-For` ヘッダが付いた接続は 403 を返す。

| パラメータ | 既定 | 意味 |
| --- | --- | --- |
| `include` | なし | `request_body` / `response_body` をカンマ区切りで指定 |
| `max_body` | `65536` | 本文を載せるときの切り詰めバイト数 |

未知の `include` 値・未知のパラメータ名・数値でない `max_body` は 400。
購読が追いつけなくなった場合、その購読は切断される (古い列へ復帰しない)。

```bash
curl -sSN 'http://127.0.0.1:8402/llm-gateway/tap?include=request_body,response_body&max_body=4096' \
  > tap.jsonl
```

```json
{"ts":1785326400,"ns":"default","model":"claude-opus-5","route":"anthropic-a","status":200,"thinking":{"type":"adaptive"},"tool_choice":"auto","stream":false,"request_body_size":2481,"response_body_size":712,"credential":"personal","origin":"main"}
```

`origin` はその 1 本を出した側。効かせた prompt cache 戦略があれば
`cache_strategy`、この 1 本が残す寿命があれば `cache_ttl_secs`、合図の戻りだった
1 本には `keepalive` が加わる。`include` を指定した購読にだけ `request_body` /
`response_body` が加わる。
切り詰め長は購読ごとに独立している。`thinking` / `tool_choice` / `stream` は
gateway が書き換える前の、クライアントが送ってきた値。

## 再認証 (login)

refresh token が失効したときに、ブラウザから OAuth をやり直す口 (DR-0023)。
**`claude_oauth` の credential だけ**が対象。`codex_oauth` はページ上で CLI の
実行を案内する。

追加の認証は置かない。書けるのは正規の認可を通った token だけで、任意値を
書き込める口ではない。横取りは state (CSRF) + PKCE + 単回使用 TTL (10 分) で防ぐ。

### `GET /llm-gateway/login`

設定に書かれた credential を列挙する HTML ページ。`claude_oauth` の行には
credential 専用ページへの「Log in」リンクだけを表示する。`codex_oauth` の行には
CLI の実行方法を表示する。

```bash
open http://127.0.0.1:8402/llm-gateway/login
```

### `GET /llm-gateway/login/{name}/start`

state と PKCE verifier を作ってメモリに保持し、credential 専用の HTML ページを返す。
ページには credential 名、別タブで開く Anthropic の認可リンク、短い手順、コード
貼り付けフォームがある。認可後に Anthropic console が表示する `code#state` をコピーし、
元のページへ戻って貼り付けて保存する。

設定に無い名前は 404、`claude_oauth` 以外の credential は 400 を返す。

### `POST /llm-gateway/login/{name}`

貼り付け方式の受け口。`application/x-www-form-urlencoded` の `code` フィールドに
`code#state` (または `#` の無いコード単体) を入れて送る。

```bash
curl -sS http://127.0.0.1:8402/llm-gateway/login/personal \
  --data-urlencode 'code=<code>#<state>'
```

成功すると「Credential `<name>` was updated.」を含む HTML を返す。空文字列や
`#` の片側が欠けた入力、期限切れ・使用済みの state は 400。

保存は CLI の login と同じ経路を通る (credential のロックを取り、既存を土台に
書き戻す)。常駐の refresh 処理と消し合わない。

## CLI コマンド対応表

```
llm-gateway <command> [options]
```

| コマンド | 内容 | 対応する口 |
| --- | --- | --- |
| `serve` | 待ち受けを開始する | (全部) |
| `check` | 設定を読んで検証する (起動はしない) | — |
| `models` | 設定に書かれたモデルを一覧する | — |
| `usage` | credential ごとの利用状況 (サーバに問い合わせる) | `/llm-gateway/usage` |
| `status` | upstream サービスの状態 (サーバに問い合わせる) | `/llm-gateway/status` |
| `stats` | credential × モデル × 日のトークン数と USD | `/llm-gateway/stats` |
| `login` | ブラウザで認可して `<name>.json` に保存する | `/llm-gateway/login` |

オプション:

| オプション | 対象 | 意味 |
| --- | --- | --- |
| `--config <path>` | 全コマンド | 設定ファイル (既定: `$XDG_CONFIG_HOME/llm-gateway/config.toml`) |
| `--refresh` | `usage` / `status` | 読み直してから表示する (`usage` では少し消費する) |
| `--days <N>` | `stats` | 直近 N 日 (既定: 7、`0` で全期間) |
| `--type <type>` | `login` | `claude_oauth` または `codex_oauth` |
| `--help`, `-h` | 全コマンド | ヘルプ |
| `--version` | — | バージョン |

環境変数:

| 変数 | 意味 |
| --- | --- |
| `LLM_GATEWAY_LOG` | ログの詳細度 (既定: `info`) |
| `XDG_CONFIG_HOME` | 設定ファイルの既定の置き場 |
| `XDG_STATE_HOME` | credential とログの既定の置き場 |
