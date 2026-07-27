# DR-0002: コンポーネント構成と段階リリース

- Status: Active
- Date: 2026-07-27

## Context

[DR-0001](./DR-0001-scope-and-architecture.md) でスコープを確定した後、実装着手前に
実機検証を行った結果、**DR-0001 の前提が 2 点崩れた**。また kawaz から
**OpenAI 系プロバイダ対応**の要求が追加された。

> cc からモデル名として gpt を指定したら、裏では OpenAI エンドポイントに繋いで欲しい。

本 DR は、実測に基づいてコンポーネント境界を確定し、段階リリースの区切りを決める。

### 崩れた前提 1: 「ボディは触らない」

Bedrock に直接投げて確認した:

| 認証ヘッダ | ボディの `model` | 結果 |
|---|---|---|
| `x-api-key` | `anthropic.claude-fable-5` | **200** |
| `x-api-key` | `claude-fable-5` | **404** `The model does not exist` |
| `Authorization: Bearer` | `anthropic.claude-fable-5` | **401** |

ルーティングキーである `model` がボディ内にあり、upstream ごとに要求する名前が違う。
**alias → upstream 名の書き換えは避けられない。**

### 崩れた前提 2: 「ヘッダを足さないので事故は起きない」

DR-0001 は「この gateway はそもそもヘッダを足さないので同種の事故は起きない」と
書いたが、これは**足す側しか見ていなかった**。実際には**落とす側**が要る:

| 送った `anthropic-beta` | Bedrock |
|---|---|
| 無し | **200** |
| クライアントが送る束をそのまま透過 | **400** `invalid beta flag` |

**実クライアント (Claude Code 2.1.220) が送る束を実測**した
(8319 に受信専用プローブを立てて観測)。送られるのは 8 個で、
うち 4 個を Bedrock が拒否する:

| フラグ | Bedrock |
|---|---|
| `interleaved-thinking-2025-05-14` | 200 |
| `thinking-token-count-2026-05-13` | 200 |
| `context-management-2025-06-27` | 200 |
| `claude-code-20250219` | 200 |
| `oauth-2025-04-20` | **400** |
| `prompt-caching-scope-2026-01-05` | **400** |
| `advisor-tool-2026-03-01` | **400** |
| `extended-cache-ttl-2025-04-11` | **400** |

llm-notes findings が測った束 (cpa の注入値) には `fast-mode-2026-02-01` /
`redact-thinking-2026-02-12` / `token-efficient-tools-2026-03-28` /
`context-1m-2025-08-07` / `structured-outputs-2025-12-15` が含まれ、
`thinking-token-count` / `advisor-tool` / `extended-cache-ttl` は含まれなかった。
**拒否フラグの集合はクライアントのバージョンで変わる**ため、
リストを固定値でハードコードすると追従できない (下記 Decision で対処)。

つまり **Bedrock 向けには拒否されるフラグの除去が要る**。完全な素通しは成立しない。

### 追加要求: OpenAI 系

実運用ログ (cpa、全期間) を見ると、**Claude Code は `gpt-5.6-sol` を選んだ時も
`POST /v1/messages` (Anthropic Messages 形式) で送っている**:

| パス | 回数 |
|---|---|
| `POST /v1/messages` | 18,138 |
| `POST /v1/chat/completions` | 2 |

モデル分布 (直近 6 万行): opus-5 2,219 / sonnet-5 1,972 / **gpt-5.6-sol 1,597** / fable-5 358。
sol の 1,597 件はすべて codex 系 auth が選択されており、cpa が
**Anthropic Messages → ChatGPT Responses API** の変換をして通していた。

Bedrock 経由の OpenAI は使えない (実測、2026-07-27 再確認):
`openai.gpt-5.6-sol` / `terra` とも 400 `does not support the '/v1/chat/completions' API`、
`gpt-oss-120b` のみ 200。**ChatGPT サブスク OAuth 経路が実質唯一。**

## Decision

### コンポーネント分割 (kawaz 案 mid=6 を採用)

```
                    ┌──────────────────────────────────────────┐
  Claude Code ─────▶│ EndpointAdapter                          │
  (ANTHROPIC_       │   Anthropic Messages API を話す口         │
   BASE_URL)        │   /v1/messages, /count_tokens, /v1/models │
                    │                    │                     │
                    │                    ▼                     │
                    │ ModelRouter                              │
                    │   model 名 → (Backend, Credential) の     │
                    │   優先順位リスト。上から試す                │
                    │                    │                     │
                    │                    ▼                     │
                    │ BackendAdapter (話す API ごと)            │
                    │  ├ AnthropicMessages                     │
                    │  │   転送 + SSE 中継の実体。差分は Provider │
                    │  │   ├ Anthropic ───────────────────────┼──▶ api.anthropic.com
                    │  │   ├ Bedrock   ───────────────────────┼──▶ bedrock-mantle.*.api.aws
                    │  │   └ Relay     ───────────────────────┼──▶ cpa (8317) ※Phase 1
                    │  └ OpenAiResponses  プロトコル変換        ┼──▶ chatgpt.com ※Phase 2
                    │                    │                     │
                    │                    ▼                     │
                    │ CredentialStore                          │
                    │   get / refresh (single-flight)          │
                    │   永続化は Persistence trait でプラガブル  │
                    │    ├ PlainFile   ← v1                    │
                    │    └ CacheWarden ← 将来                   │
                    └──────────────────────────────────────────┘
```

### アダプタ間の中間表現は Anthropic Messages 形式とする

入口が Anthropic Messages 形式の 1 種類しかないため、**独立した IR を定義しない**。

- Claude 系バックエンド (OAuth / Bedrock) は変換ゼロで通る
- OpenAI バックエンドは、**そのアダプタの内部で**双方向変換を閉じる

独立 IR の利点 (入口が N 種類・出口が M 種類の時に N×M → N+M) は、入口が増えた時に
初めて効く。現状クライアントは Claude Code だけで、その予定もない。
変換を OpenAI アダプタ内部に閉じておけば、後から IR を抜き出すのは局所的な作業で済む。

### BackendAdapter は「話す API」で分け、その中をプロバイダで分ける

kawaz 裁定 (mid=15, mid=16):

> anthropic バックエンドアダプタの亜種で別アダプタという建て付けが良いでしょう。
> bedrock は一皮ラップしてるだけのバックエンドアダプタ? プロバイダ? でしょ。

**アダプタは「どの API を話すか」で分ける**。実測で、Bedrock は Anthropic
Messages API をそのまま提供しており (SSE の形式まで同一)、公式との違いは
接続先・認証方式・モデル名の接頭辞・beta の受理範囲だけだった。
つまり Bedrock は **Anthropic アダプタを一皮ラップしたプロバイダ**であって、
別種のアダプタではない。

```
BackendAdapter (話す API ごと)
├ AnthropicMessages    Messages API を話す。転送と SSE 中継の実体はここが持つ
│   ├ Anthropic  api.anthropic.com / Bearer / 変換なし
│   ├ Bedrock    bedrock-mantle.*  / x-api-key / 接頭辞 + beta 除去
│   └ Relay      別 gateway        / 認証なし    ※Phase 1 の cpa 転送
└ OpenAiResponses      Responses API を話す。プロトコル変換を持つ ※Phase 2
```

`AnthropicMessages` が持つ実体: リクエストの受領、upstream への転送、
レスポンスヘッダの受領、**SSE のバイト列中継**、フォールバック境界の判定。
これらは upstream が Messages API である限り同一。

プロバイダは trait 実装として書く:

```rust
/// Messages API を話す upstream ごとの差分。
trait AnthropicProvider {
    fn endpoint(&self) -> &Url;
    /// 認証ヘッダを載せる。方式は実装ごとに違う (Bearer / x-api-key / 無し)。
    async fn authorize(&self, req: &mut RequestParts) -> Result<()>;
    /// upstream の要求に合わせる (model 名の変換、拒否 beta の除去など)。
    /// 既定は何もしない = 公式 Anthropic はこれで済む。
    fn adapt(&self, _req: &mut MessagesRequest) {}
}
```

**差分をパラメータ化しない** (kawaz 裁定 mid=18)。

> パラメータ調整可能にしたとして無限に要求が増える上に使うパラメータの
> 組み合わせなんてプロバイダごとに 1 個ずつしかないんだから、全ての組み合わせ
> 自由度は無駄な上に学習コストが上がるだけ (しかも無意味に)

`{endpoint, auth_scheme, model_map, beta_policy}` のような設定 struct にすると、
理論上は 4 つの直交する軸になる。だが**実際に使われる組み合わせは
プロバイダごとに 1 個ずつしかない** (Bedrock は必ず x-api-key + 接頭辞 +
beta 除去のセットで、その一部だけ違う構成は存在しない)。
使われない自由度の代償は 3 つ:

- 設定スキーマを覚える学習コスト
- 「x-api-key なのに beta 除去なし」のような**成立しない組み合わせが書ける**余地
- 軸が足りなくなるたびに軸が増える (= 無限に要求が増える)

trait なら、プロバイダ 1 つ = 実装 1 つで対応が閉じる。
実際に必要な組み合わせだけがコードに現れ、不正な組み合わせは表現できない。

同じ理由で、`adapt` の中身を `bool` で分岐させることもしない。
処理そのものを実装に書かせれば、共通側は upstream の事情を知らずに済む。

一方 `OpenAiResponses` が別アダプタなのは、**共有できる実体が無い**ため。
リクエスト・レスポンスとも別スキーマで、SSE はイベント列の再構築が要る
(バイト列中継ができない)。ここを同じ trait に押し込むと、
`AnthropicMessages` の中にまったく別の実装が同居することになる。

### `anthropic-beta` の除去は「拒否リスト + 自己修復」で行う

拒否フラグの集合は**クライアントのバージョンで変わる** (上記 Context の実測)。
固定リストだけでは、Claude Code が新しい beta を足した日に fable-5 が全滅する。
二段構えにする:

1. **既定の拒否リストを設定で持つ** (コードに初期値、設定ファイルで上書き可)。
   通常はこれで除去され、upstream への往復は 1 回で済む
2. **`invalid beta flag` の 400 を受けたら、エラー本文から該当フラグを特定して
   除去し、1 回だけ再試行する**。成功したらそのフラグを**実行時の拒否リストに
   学習させる** (プロセス内メモリ。再起動でリセット)

2 があることで、未知のフラグが増えても自動復旧する。1 があることで、
平常時に余計な往復が起きない。

**除去は Bedrock 経路に限る**。Anthropic 公式経路では全フラグが受理されるので、
そこで落とすと機能を失う (`fast-mode` を落とすと fast mode が効かない等)。

なお 400 応答から常にフラグ名を特定できるとは限らない
(実測した応答は `{"error":{"message":"invalid beta flag"}}` でフラグ名を含まない)。
その場合は**拒否リスト適用後の全フラグを落として再試行**する
(= 最後の手段。機能は失うがリクエストは通る)。

### フォールバックは「最初のバイトを送るまで」

優先順位リストを上から試す (DR-0001 の機能 3) が、**切り替えられる期間には限界がある**。
クライアントへレスポンスを 1 バイトでも送出した後は、HTTP レスポンスを
やり直せないため、次の upstream に切り替えられない。

境界を明示する:

| 失敗のタイミング | 挙動 |
|---|---|
| 接続確立・TLS・upstream の HTTP ステータス受信まで | **次の upstream を試す** |
| ステータス受信後、クライアントへ送出する前 (非 stream の全文取得中) | **次の upstream を試す** |
| クライアントへ送出開始後 (SSE の途中で切断) | **切り替えない**。ストリームを終了し、エラーを SSE イベントとして通知する |

つまり **upstream のレスポンスヘッダを受け取ってからクライアントへ書き始める**
実装にすることで、フォールバック可能な期間を最大化する。
SSE の場合、最初のイベントを転送した時点で確定する。

フォールバックの発動条件は **経路断のみ** (接続失敗 / タイムアウト / 5xx)。
429 では発動しない (DR-0001: レート制限の分散と経路断のフォールバックは別物)。

**設定に書かれた経路は、内部エラーで消さない**。cpa は稼働中に fable-5 の
Bedrock 登録を内部で失い、`/v1/models` から消滅して `no auth available` の
503 を返す状態になった (2026-07-27 19:29 実測。設定ファイルも upstream も正常、
プロセス再起動なし)。**設定にある経路は常に試行対象として残し**、
連続失敗は「その経路を一時的に後回しにする」までに留める
(= 候補から除去しない)。cpa と同じ形の障害を作らないための不変条件。

### Claude 系の SSE はバイト列中継でよい (実測)

OAuth token を直投げして確認: `message_start` / `content_block_start` /
`content_block_delta` / `content_block_stop` / `message_delta` / `message_stop` の
生イベント列がそのまま返る。**パースも再構築も不要。**

これにより Anthropic 系アダプタはストリームを触らずに済み、
レイテンシとメモリを upstream 直結と同等に保てる。

### session affinity は ModelRouter が担う

DR-0001 の必須機能 2 (session → auth 固定) の置き場所を確定する。
**ModelRouter の責務**とし、「model 名 + session キー」から
(Backend, Credential) を決める。session キーの導出は EndpointAdapter が行い、
リクエストのメタデータとして Router に渡す。

キーの導出仕様は cpa の `sdk/cliproxy/auth/selector.go` (`extractSessionIDs`) に倣う。
**DR-0001 は「`X-Claude-Code-Session-Id` ヘッダ」と書いたが、cpa の affinity は
そのヘッダを見ていない**。実際の優先順位は:

| # | ソース | キー |
|---|---|---|
| 1 | ボディ `metadata.user_id` の `_session_<uuid>` パターン | `claude:<uuid>` |
| 2 | ボディ `metadata.user_id` が JSON なら その `session_id` | `claude:<uuid>` |
| 3 | `X-Session-ID` ヘッダ | `header:<id>` |
| 4 | `Session-Id` / `Session_id` ヘッダ | `codex:<id>` |
| 5 | `X-Client-Request-Id` ヘッダ | `clientreq:<id>` |
| 6 | `metadata.user_id` (上記パターン外) | `user:<id>` |
| 7 | ボディ `conversation_id` | `conv:<id>` |
| 8 | メッセージ内容のハッシュ (フォールバック) | `msg:<hash>` |

Claude Code は 1 で解決される (ログの `session=claude:...` 17,644 件がこれ)。

8 のフォールバックは **system prompt + 最初の user + 最初の assistant の
FNV-1a 64bit ハッシュ**:

```
msg:%016x  ←  fnv64a("sys:" + systemPrompt + "\n" + "usr:" + userMsg + "\n" + "ast:" + assistantMsg + "\n")
```

初回は assistant が無いので短縮ハッシュ (sys + usr) を返し、2 ターン目以降は
完全ハッシュを primary、短縮を fallback として**初回の binding を引き継ぐ** 2 段構え。
実運用での出現は 161 件 (0.9%、curl 等) なので、v1 は
**短縮ハッシュのみに単純化する**。会話が進んでも同じキーになり、
affinity としてはむしろ安定する (完全ハッシュ方式が必要なのは、
異なる会話の初回リクエストが衝突するのを避けたい場合)。

なお cpa には `claude:<session>:agent:<agent-id>` という別のキー
(`ClaudeCodeExecutionScope`) もあるが、これは **codex 経路の実行状態と
prompt cache 用**であって affinity のキーではない。混同しないこと。

### CredentialStore は get/set でなく「用途」を持つ

DR-0001 の `SecretStore { get, set }` を改める。OAuth token は 8 時間で失効し
**リフレッシュのたびに `refresh_token` がローテートする**ため、
素の get/set だと呼び出し側が競合制御を負う:

```rust
trait CredentialStore {
    /// 有効な認証情報を返す。失効間近なら内部でリフレッシュする。
    /// 同一 credential への並行呼び出しは 1 回のリフレッシュに束ねられる。
    async fn acquire(&self, id: &CredentialId) -> Result<Credential>;
}

/// 永続化だけを担う。こちらが差し替え対象。
trait Persistence {
    fn load(&self, id: &CredentialId) -> Result<StoredCredential>;
    fn store(&self, id: &CredentialId, value: &StoredCredential) -> Result<()>;
}
```

**single-flight は必須** (kawaz mid=7「当たり前にやること」)。
cpa も `singleflight.Group` を使っており、`refresh_token_reused` エラーの
検出コードを持つ = ローテート方式であることの裏付け。二重リフレッシュは
後発が失敗して**全アカウント再ログイン**を招く。

`Persistence` の v1 実装は `PlainFile` (kawaz 確認 2026-07-27):

- 保存先: **`~/.cache/llm-gateway/auth/`** (`$XDG_CACHE_HOME` 配下)
- 形式: **cpa 互換 JSON**
  (`{type, email, access_token, refresh_token, expired, last_refresh, priority, disabled, excluded-models}`)
- 初期投入: cpa の `auth-personal/*.json` からコピーする。
  **元ファイルは触らない**ので cpa と併存できる
- リフレッシュを試す前のバックアップは
  `~/.cache/llm-gateway/auth-backup/<timestamp>/` に取る (取得済み)

cache-warden の永続化が固まったら `CacheWarden` 実装を足して差し替える。

**移行後に得られるのは暗号化だけではない**。cache-warden は Unix socket の
接続相手を署名で検証できる (`macos-process-inspect` の `verify_peer`)。
平文ファイルは「同じ UID なら誰でも読める」のに対し、cache-warden 経由なら
**同じ Team ID で署名されたバイナリからしか取得できない**状態を作れる。
gateway 本体に署名する理由もここにある (上記「配布しない」節)。

つまり `Persistence` の 2 実装は「同じことを別の場所でやる」のではなく、
**アクセス制御の有無が違う**。v1 が平文なのは cache-warden 側の準備待ちであって、
この差を許容し続ける判断ではない。

### OAuth リフレッシュの仕様 (cpa v7.2.100 のソースで確定)

| | Anthropic (claude) | ChatGPT (codex) |
|---|---|---|
| token URL | `POST https://api.anthropic.com/v1/oauth/token` | `POST https://auth.openai.com/oauth/token` |
| client_id | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | `app_EMoamEEZ73f0CkXaXp7hrann` |
| ボディ形式 | **JSON** `{client_id, grant_type: "refresh_token", refresh_token}` | 同形 |
| 応答 | `{access_token, refresh_token, expires_in, account.email_address}` | 同形 + `id_token` |
| token 寿命 | 8 時間 (実測) | 10 日 (実測: 07-23 → 08-02) |
| ローテート | **する** (応答の refresh_token で上書き) | **する** (`refresh_token_reused` 検出あり) |

リフレッシュ実行前に auth JSON をバックアップする
(DR-0001 Consequences。実装ミスが全アカウント再ログインを招くため)。

### ChatGPT サブスク経路の接続仕様 (Phase 2 用、cpa ソースで確定)

- エンドポイント: `https://chatgpt.com/backend-api/codex/responses` (**Responses API 形式**)
- 必須ヘッダ: `Authorization: Bearer <access_token>`, `Chatgpt-Account-Id: <account_id>`,
  `Originator: codex_cli_rs`, `Session_id`, `X-Codex-Beta-Features`,
  `Accept: text/event-stream` (stream 時)
- **この経路は素通しではない**。Claude 系と違い、クライアント偽装に相当するヘッダが要る
  (DR-0001 の「偽装しない」方針は Anthropic 経路についての判断であって、
  ChatGPT 経路には適用できない — upstream が Codex CLI 用の口しか公開していないため)

### 段階リリース (kawaz 確認 2026-07-27)

| Phase | 内容 | cpa の扱い |
|---|---|---|
| **1** | Claude 系 (OAuth プール + Bedrock) を自前実装。`gpt-*` は Relay プロバイダで cpa (8317) へ転送 | 8317 で稼働継続 |
| **2** | Anthropic ⇄ Responses 変換を自前実装し、ChatGPT 直結に切替 | 停止可能になる |

Phase 1 の時点で、直近 6 万行のモデル別内訳
(opus-5 2,219 + sonnet-5 1,972 + fable-5 358 + haiku 12 = **4,561**、
対して sol 1,597、総計 6,158) のうち **74% が自前 gateway を通る**。
残る sol は cpa 経由なので、cpa の beta 注入問題が sol 経路に残る (現状 実害なし)。

**転送構成の副作用の扱い**:

- **ループは起きない**。cpa (8317) の upstream 設定に gateway (8319) は存在しない。
  転送は一方向
- **session affinity は二重適用しない**。`gpt-*` については gateway 側で affinity を
  持たず、cpa の `session-affinity: true` / TTL 1h に委ねる。gateway が独自に
  auth を選ぶわけではない (転送するだけ) ので、選択の主体は cpa 側の 1 つに保たれる
- **failover も二重管理しない**。同じ理由で、`gpt-*` の経路断対応は cpa 側の挙動に従う

変換の規模 (cpa v7.2.100 実測):
`internal/translator/claude/openai/responses/` は本体 **1,758 行**
(request 834 + response 924)、テスト込み 2,940 行。Phase 1 と分ける根拠はここ。

### 配布しない (kawaz 裁定 2026-07-27)

> 当面配布しない。個人用。

これに伴い、kawaz リポの標準構成から以下を**持たない**:

| 通常やること | 本リポでの扱い |
|---|---|
| GH Release / tag / 配布 artifact | **無し**。`.github/workflows/release.yml` を作らない |
| `check-version-bumped` gate | **無し**。リリースしないので version を進める意味がない |
| README / DESIGN の英訳ペア | **無し**。日本語のみ (公開・配布しないため) |
| `.app` bundle | **無し** |
| **codesign** | **する** (kawaz 裁定 mid=20) |
| notarize | **無し**。ローカルビルドは quarantine されないので不要 |
| launchd 登録 | **持つ**。常駐は要る (下記) |

`push = 完了` として扱う (release workflow を持たないリポの標準)。

**バイナリには Apple 署名をする** (kawaz 裁定 mid=20, mid=21)。
配布のためではなく、**cache-warden の peer 認証に乗るため**:

> cache-warden は get 要求時にソケット通信のプロセスの capability チェック機能を
> モデル設計になっていて、署名のチーム ID 一致するバイナリからのみ取得可能の
> ような設定が出来るのでそこに乗せると色々セキュリティ面が簡単に楽にできる。

cache-warden の `macos-process-inspect` は、Unix socket の接続相手を
`peer_identity(fd)` / `verify_peer(fd, prefix)` で検証できる (実装済み)。
署名しておけば、秘密の取得元を「同じ Team ID で署名されたバイナリ」に
絞れる。**これは平文ファイルでは得られない性質**で、
`CacheWarden` バックエンドが単なる「暗号化された保存先」ではなく
**アクセス制御を持つ保存先**であることを意味する。

`just build` の中で `codesign` を実行する。identity は `CODESIGN_IDENTITY` env で
上書き可、既定はローカル keychain の Developer ID Application を自動検出
(cache-warden の `approver-run` recipe と同じ形)。dev build も実 identity で
署名する — ad-hoc 署名では Team ID が付かず peer 認証を通れないため。

常駐は launchd で行う。**plist から直接バイナリを起動する** (ラッパを挟まない)。

cpa は「plist → `start.sh` → バイナリ」の 3 段だったが、これは
**cpa 固有の制約への対処**であって真似する理由がない。cpa はログの出力先を
コードで決め打っており設定から変えられないため、ラッパで `WRITABLE_PATH` を
用意してから `exec` する必要があった。llm-gateway は自分で書くので、

- ログ先は plist の `StandardOutPath` / `StandardErrorPath` で指定する
- そのディレクトリはバイナリ自身が起動時に作る

とすれば 2 段で済む。段を減らすほど、起動失敗時に見る場所が減る。

`.app` bundle 方式 (cache-warden) は採らない。あれは TCC 権限 (TouchID) が
要る cache-warden 固有の事情によるもので、本リポには不要。

### 技術選定

| 項目 | 選択 | 根拠 |
|---|---|---|
| HTTP サーバ | axum 0.8 (`default-features = false`) | kawaz/hyoui の `hyoui-web` と同じ。SSE / streaming body を扱える |
| HTTP クライアント | reqwest 0.13 (`rustls-tls`, `stream`, `http2`) | ストリーム透過に `stream` feature が要る |
| 非同期 | tokio 1 (`rt-multi-thread, macros, net, signal, sync, time, fs`) | 同上 |
| エラー | thiserror 2 | cache-warden / hyoui と共通 |
| 秘密 | zeroize 1 | cache-warden と共通。Drop 時消去 |
| ログ | tracing 0.1 + tracing-subscriber | |
| 設定 | toml 0.9 + serde | hyoui と共通 |
| CLI | 自作パーサ | cache-warden / hyoui とも clap 不使用。`cli-design-preferences` の階層 help を自前で満たしている |
| edition | 2024 / rustc 1.97.1 | |

crate 分割は hyoui に倣う:

```
crates/llm-gateway         コア (router / backends / credentials)。HTTP 非依存
crates/llm-gateway-server  axum で口を生やす層
crates/llm-gateway-cli     バイナリ + サブコマンド (serve / auth / status)
```

### エンドポイントの実装範囲 (実運用ログから決定)

| パス | 実測回数 | 実装 |
|---|---|---|
| `POST /v1/messages` | 18,138 | **要** |
| `POST /v1/messages/count_tokens` | 157 | **要** (通常 200、503 は fable-5 停止時の巻き添え) |
| `GET /v1/models` | 30 | **要** (モデルピッカー) |
| `HEAD /api/hello` | 128,135 | 不要 (cpa は 404 を返しており支障が出ていない) |
| `/v0/management/*` | 1,000+ | 不要 (cpa の管理 GUI 用) |

## Alternatives Considered

- **案 A: 独立した中間表現 (IR) を定義する**
  - 不採用理由: 入口が Anthropic Messages 1 種類なので N×M 問題が発生しない。
    IR を挟むと Claude 系で不要な往復変換が生じ、SSE のバイト列中継ができなくなる
    (レイテンシとメモリで損をする)。入口が増えた時に抜き出せばよい
- **案 B: Phase 1 から OpenAI 変換も実装する**
  - 不採用理由: 1,758 行の変換が完成するまで移行を始められない。
    `AnthropicRelay` は捨て駒ではなく、将来 work 面など別 gateway に
+    Relay プロバイダは捨て駒ではなく、業務面など別 gateway に
    転送したい時に同じ実装が使える
- **案 C: Bedrock 向けに `anthropic-beta` を丸ごと落とす**
  - 不採用理由: Bedrock が受理する 5 機能 (`context-1m` / `context-management` /
    `interleaved-thinking` / `structured-outputs` / `claude-code`) まで失う。
    cpa が束ごと置換して `context-management` を落とし実障害を出した事例がある
    (llm-notes DR-0001)。**拒否リストで落とす側を最小にする**
- **案 D: `CredentialStore` を DR-0001 どおり get/set のままにする**
  - 不採用理由: refresh_token がローテートする以上、「失効判定 → リフレッシュ →
    保存」を呼び出し側に書かせると競合制御が漏れる。二重リフレッシュの代償が
    全アカウント再ログインなので、store 側で束ねる
- **案 F: プロバイダ差分を設定 struct でパラメータ化する**
  (`{endpoint, auth_scheme, model_map, beta_policy}` を設定ファイルに書き、
  プロバイダ追加をコード変更なしで済ませる)
  - 不採用理由: **実際に使われる組み合わせはプロバイダごとに 1 個ずつしかない**。
    Bedrock は必ず「x-api-key + モデル名接頭辞 + beta 除去」のセットで、
    その一部だけ違う構成は存在しない。理論上の直交軸が実運用で使われない以上、
    自由度は学習コストと不正な組み合わせの余地を増やすだけになる。
    軸が足りなくなるたびに軸が増える (要求が無限に増える) 問題もある。
    trait なら「プロバイダ 1 つ = 実装 1 つ」で対応が閉じ、
    成立しない組み合わせは表現できない
- **案 E: cpa と同じく管理 GUI を持つ**
  - 不採用理由: DR-0001 のスコープ外。auth の確認は CLI サブコマンドで足りる

## Consequences

- **DR-0001 の「ボディを触らない」は撤回**。`model` フィールドのみ書き換える。
  それ以外のボディ要素には触れない方針は維持する
- **DR-0001 の「ヘッダを足さない」も不十分だった**。Bedrock 向けには
  `anthropic-beta` から拒否フラグを**除去**する。除去リストは設定で上書き可能にし、
  既定値をコードに持つ (upstream 変更への追従性のため)
- **ChatGPT 経路は偽装を伴う**。`Originator: codex_cli_rs` 等は Codex CLI を
  名乗るヘッダで、DR-0001 の「偽装しない」原則の例外になる。
  upstream が他の口を公開していないため回避手段がない
- **Phase 1 の間は cpa への依存が残る**。`gpt-*` が cpa 経由なので、cpa を止められない。
  cpa の beta 注入問題も sol 経路に残る (現状 実害なし)
- **v1 は秘密が平文でディスクに残る** (DR-0001 から変更なし)。
  `~/.cache/llm-gateway/auth/` に cpa 互換 JSON
- SSE をバイト列中継する設計上、**Claude 系のレスポンスに対する加工は将来も入れにくい**。
  加工が必要になった時は、その経路だけパース層を挟む形になる

## 実測で確定した前提 (本 DR のための検証、2026-07-27)

| 検証 | 結果 |
|---|---|
| Bedrock: `x-api-key` + upstream モデル名 | 200 |
| Bedrock: `x-api-key` + alias 名 | **404** |
| Bedrock: `Authorization: Bearer` | **401** |
| Bedrock: `anthropic-beta` 無し | 200 |
| Bedrock: クライアントの beta 束を透過 | **400** |
| Bedrock: 拒否される beta フラグ | 10 中 **5** |
| Bedrock OpenAI 互換: `openai.gpt-5.6-sol` | **400** (`gpt-oss-120b` のみ 200) |
| Anthropic: OAuth token 直投げ (system prompt 無し) | 200 |
| Anthropic: SSE 生イベント列の透過 | 可 |
| `POST /v1/oauth/token` の存在 | 400 (パラメータ不足の正常応答、token 未消費で確認) |

## 関連

- [DR-0001](./DR-0001-scope-and-architecture.md) — スコープ (本 DR が前提の一部を改訂)
- `kawaz/llm-notes` DR-0002 — 自作を決めた判断
- `kawaz/llm-notes` findings/2026-07-27-bedrock-api-key-integration.md — beta フラグの一次検証
- `kawaz/hyoui` `crates/hyoui-web` — axum 構成の参考
