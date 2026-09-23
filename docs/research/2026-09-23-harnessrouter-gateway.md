# harnessrouter の gateway 実装 — routing / 認証情報 / 失敗の返し方

対象: HarnessRouter/harnessrouter (Apache-2.0) の `gateway/`。比較: kawaz/llm-gateway、kawaz/ccmsg。調査日 2026-09-23、読み取りのみ (実行はしていない)。

## 1. 対象の要約 (事実)

### 1.1 全体の形

- 製品の位置づけ: Codex / Claude Code 等の「ハーネス (agent CLI)」を sandbox 内で走らせ、1 本の API (独自プロトコル UHP + OpenAI Responses 互換) で task を投げ・結果を取り・ハーネスを切り替えられるようにする基盤 (`README.md`)。LLM API の proxy ではなく、**agent 実行の orchestrator** であり、LLM 呼び出しはその内側で副次的に中継される。
- gateway は Python (FastAPI)。ほぼ全部が `gateway/app.py` 1 ファイル (15,892 行) に入っている。他は `backing.py` (580 行、永続化の差し替え口)、`control_store.py` (542 行、idempotency / lease / cancel)、`control_sqlite.py`、`media_plane.py`、`sql_plane.py`。
- ハーネス本体は別プロセスの `runner/` が sandbox で実行し、gateway は `/turn/{id}` を HTTP で叩いて poll する (`app.py` の `_sandbox_json`、`_resp_execute`)。
- プロトコル仕様は `protocol/` (`versions/2026-09-12/*.md`、OpenAPI、`conformance/`) に独立して置かれている。

### 1.2 ハーネス選択と routing

- 入口は `POST /v1/responses` (`create_response`)。model 名や明示指定から backend (= ハーネス種別) を決める `_route_backend` は文字列の部分一致 (`"claude"`/`"opus"` → claude、`"gpt"`/`"o3"` → codex 等) で、該当なしは `DEFAULT_BACKEND`。
- friendly model 名 → provider 実 ID の対応表を provider ごとに dict で持つ (`_BEDROCK_CLAUDE`、`_ANTHROPIC_CLAUDE` 等)。
- 接続 (connection = provider の認証情報 1 件) の試行順は `_resolve_chain`: 明示指定 > org の policy > global policy。policy は vault に `harness-policy-<backend>` として `{"chain": [...]}` か `[...]` で保存 (`_policy_chain` は両形を読む)。model 対応の integration が先頭に差し込まれ、policy chain はその後ろの fallback になる (`_resp_execute` 内 `candidates`)。

### 1.3 認証情報 (provider key) の扱い — egress broker

`app.py` の「LLM egress broker」節 (`_auth_from_conn`、`llm_broker`、`_mint_turn_cred`) が中核。

- 動機はコメントに明記: sandbox は顧客の agent が bash とネットワーク付きで走るので、渡したものは公開されたとみなす。実際に共有 key が `printenv` で抜かれ 6 日間課金された事故 (2026-07-25) がある。
- sandbox には **provider key を渡さない**。代わりに per-turn token `hrt_<base64(sid|conn_name|exp)>.<hmac>` と、gateway 自身の `/v1/llm` を base_url として渡す。token は HMAC (共有鍵 `HARNESS_INTERNAL_KEY`) なので replica 間で状態共有なしに検証でき、TTL (既定 6h) で失効する。
- `llm_broker` (`/v1/llm/{path}`) は token を検証 → sid と接続名から実 credential を引き直し → provider ごとの流儀 (`x-api-key` / `api-key` / `Authorization: Bearer`) で差し替えて upstream にストリーム転送する。クライアントの提示形式も Bearer / api-key / x-api-key の 3 種を受ける (`_broker_token`)。
- path は推論系の allowlist で検査 (`_broker_path_allowed`、`HR_BROKER_PATHS=enforce` で 403)。
- **fail-closed の順序**: `_auth_from_conn` はまず秘密フィールド (`_SECRET_AUTH_FIELDS`) を無条件に全部消し、その後で broker token を入れる。broker できない条件 (provider 名の揺れ、sid 空、公開 URL 未設定) で素の key が渡る fail-open を過去に踏んだので順序を逆にした、とコメントにある。broker できない接続は chain からスキップする。
- 単一テナントの self-host 用に `HR_SANDBOX_TRUST=owner` の明示 opt-in でだけ素の key を渡す。「broker に失敗したら素通し」という fallback にはしないと明記。
- provider native API しか話さない backend (`_NATIVE_ONLY_BACKENDS = {"gemini"}`) は broker 対象外として拒否。
- 保存: `backing.py` の `SecretStore` (local は SQLite/env/file、本番は vault サービス)。tenant 名は org id を正規化 + sha1 短縮で作る (`_org_tenant`)。解決順は org 自身の tenant → 共有プール (`_tenants_for`)。一覧表示は秘密フィールドを除いて返す (`_conn_public`)。
- ローテーション専用の仕組みは見当たらない (key は admin API で置き換え。per-turn token の方は TTL で自然失効)。
- 同じ HMAC 形式で「別ハーネスを操作するハーネス」用の scope 付き credential (`hrc_...`、`_calibration_route_allowed` で許可 route を正規表現列挙) も作っている。

### 1.4 リクエスト形式の変換

- 外向き API は OpenAI Responses 互換。内部ではハーネスの出すイベントを `_RespTranslator` で Responses のイベント列に翻訳する (出力側の変換)。
- broker を通る upstream 呼び出しは基本パススルーで、provider 固有の補修だけ入る: `_strip_unsupported` (provider が受けないフィールドを落とす)、Google の OpenAI 互換口が未知フィールドで 400 を返すと、エラー本文から項目名を読んで除去して最大 4 回送り直す、Google の tool call signature を応答から拾って次のリクエストに再注入する (`_GoogleSigTap`)、Gemini 向け JSON schema の書き換え。

### 1.5 失敗の分類と返し方

- UHP のエラー envelope: `{"error": {type, code, message, param, detail}, "detail": message}` (`uhp_error`、`_uhp_http_exception`)。`detail` 文字列は旧クライアント向けの deprecated alias として併記。
- type は HTTP status から機械的に決まり (`_UHP_STATUS_TYPE`)、code は raise 側が指定 (無指定なら status 由来の汎用 code)。コード表と retry 可否表は `protocol/versions/2026-09-12/errors.md` §3–4 にある (例: `rate_limited` 429 は Retry-After 後に再試行可、`quota_exhausted` 429 は再試行不可、`session_busy` 409 は完了後に再試行可、`invalid_request_error` と `authentication_error` は不可)。
- 実行中の失敗は HTTP エラーではなく **failed response** (`harness_error` / `provider_error` / `timeout`) として task の終状態に載る。
- 全 response に `UHP-Version` ヘッダ、リクエスト側の `UHP-Version` が非対応なら 400 `unsupported_protocol_version` + `detail.supported` (`_uhp_version`)。`GET /v1/uhp` は無認証の discovery で versions と capabilities (全キー必須、false も明示) を返す。
- 失敗文言: `_turn_failure_message` は「実際に走った最後の接続の理由」を文にする。試行リストの JSON (内部の接続名入り) をそのままユーザに見せていた事故から、生の tried list は出さない方針。

### 1.6 fallback / cancel / timeout

- chain の fallback (`_resp_execute`):
  - 接続が見つからない / その model を出せない / broker できない → 記録して次へ
  - 実行して失敗し、**エラー 1 行目が provider の key 拒否 (401/403/429/quota/invalid api key 等、`_provider_refused` の正規表現)** なら **次へ行かず失敗で止める**。理由はコメントに: 「拒否は設定の問題で障害ではない。次の key で走らせるとユーザはこの key が動いていると思い込む」
  - それ以外の失敗 (一時エラー等) は次の接続へ
  - `cancelled` / `timeout` は task の終わりであって、次の接続で再実行しない (全体を再実行することになるため)。timeout は `incomplete` (reason=timeout) に写す
- cancel: `POST /v1/responses/{id}/cancel` と session 単位 cancel。`control_store` の response 項目に **一方向ラッチの terminal** を持ち、終状態から live に戻る書き込みを拒否する (monotonic cancel)。turn ループは poll 10 回に 1 回ラッチを見て sandbox に kill を送る。sandbox 起動と cancel が競合した場合も running で上書きしない。
- idempotency: `Idempotency-Key` ヘッダ (または metadata) をリクエスト hash と組で予約 (`create_item` の原子的作成、重複は 409 で勝者を replay、同 key で本文が違えば 409)。store 不通時は 503 で fail-closed。runner への呼び出しにも `resp_id:conn_name` の idempotency key を付け、再送で CLI が二重実行されないようにしている。
- lease + fencing: session ごとに単調増加の `fence` を turn ごとに上げ、checkpoint は自分の fence が最新のときだけ書く (古い writer の書き込みを捨てる)。
- timeout: turn ごとに `timeout_seconds` (wall-clock cap)。プロトコル側は「streaming は総時間 timeout を持たず無活動 timeout、サーバは 30 秒ごとに `: keep-alive` を送るべき」「諦めたクライアントは task が止まったと仮定するな、明示 cancel せよ」(`errors.md` §5)。

### 1.7 永続化の切り方

- `backing.py`: **GraphStore / BlobStore / SecretStore の 3 つの Protocol**。interface は意味論 (get/upsert/find/add_edge) で切り、呼び出し側は query 文字列を組まない。実装は local (SQLite + ファイル) と vg (Cosmos graph + Azure blob + vault) の 2 つ、env 1 つで選ぶ。
- `control_store.py`: 上とは別に「熱い制御状態」(idempotency 予約 / lease と fence / response の終状態ラッチ) だけを原子的作成・ETag CAS・TTL のある store に分離。「正しさが要るところは fail-closed (idempotency)、最適化だけのところは best-effort (lease/heartbeat)、判断は各呼び出し側」と docstring に明記。`COSMOS_ENDPOINT` 未設定なら無効化し従来動作 (`control_sqlite.py` がローカル実装)。

## 2. llm-gateway が参考にできる点

### 2.1 永続化を「一貫性の意味論」で切り、熱い制御状態を別 store にする

- harnessrouter (事実): 1.7 のとおり。データ (Graph/Blob/Secret) と制御状態 (idempotency/lease/latch) を別 interface にし、制御状態の方に原子的作成・CAS・TTL を要求している。fail-closed か best-effort かを呼び出し側で決める。
- llm-gateway の現状 (事実): `docs/issue/2026-09-15-store-layer-for-replaceable-persistence.md` (open、着手指示なし) が、単一 writer 更新 (credential refresh) / リース (keepalive 発火担当) / 合算カウンタ (stats) / LWW スナップショット (usage・締め出し) の 4 意味論で trait を切る構想。現実装は全部ファイル + flock (DR-0010、DR-0027)。
- 評価: 切り方の考え方はほぼ同じで、issue の方向性の裏付けになる。追加で取り込めそうなのは 2 点。(a) 「その操作が失敗したら fail-closed か best-effort か」を trait の契約に書く (credential refresh の排他は fail-closed、keepalive リースは best-effort、など)。今の issue 本文にはこの軸が無い。(b) harnessrouter のリースには **fencing token** があり、古い担当者の遅れた書き込みを捨てる。Raft 等に差し替えた時に keepalive リースが「期限切れ後に元担当が送ってしまう」問題を防ぐ標準手段なので、リース trait の意味論に fence を含めておく価値がある。
- 取り込まない方がよい点: harnessrouter は Graph/Blob/Secret という**データの形**でも切っている。llm-gateway の issue は意味論だけで切っているので、形の軸を足すと trait が 2 軸になり過剰。形の分割は真似しない方がよい。

### 2.2 認証情報を「渡さない」broker と fail-closed の順序

- harnessrouter (事実): 1.3 のとおり。秘密を先に全部消してから許可されたものだけ足す順序、per-turn の HMAC token、「broker 失敗時に素通し」を fallback にしない、self-host の素通しは明示 opt-in。
- llm-gateway の現状 (事実): gateway 自体が credential を保持しクライアントの認証を差し替える側で、そもそもクライアントに provider credential を渡さない構造 (DR-0025「認証だけ差し替え」、DR-0030 §2 で任意ホストに一般化予定)。クライアント認証は ns のトークン。
- 評価: 「key を下流に渡さない」は llm-gateway では構造上すでに成立しているので、broker という仕組み自体は不要。取り込む価値があるのは **fail-closed の書き方**の方で、DR-0030 の汎用パススルー実装時に「クライアントの `Authorization` / `x-api-key` / `api-key` 等を落としてから登録 credential を載せる」を、**落とす対象を 1 箇所のリストで持ち、載せる分岐と独立させる**形にすると、分岐条件のミスで客側ヘッダが上流に漏れる / 登録 credential が載らず客側のものが通る、の両方を防げる。harnessrouter がこの形に至った経緯 (分岐の中で消していたので分岐が偽になる全経路で漏れた) は、そのまま DR-0030 の実装時の注意として使える。
- もう 1 点: 下流 (sandbox 等) に短命で scope 付きの credential を配る HMAC token (`hrc_`、route allowlist を token 種別に紐づける) は、DR-0030 §4 の「ns ごとの allowlist」と「ns 認証が dummy でよいのは読み取り系に閉じる場合だけ」に対して、**ns トークンを短命・scope 付きで発行する**という選択肢を与える。DR-0030 の未確定節で JWT が挙がっているなら、その比較材料になる (HMAC 自前 vs JWT)。
- 取り込まない方がよい理由: 多テナント前提の per-turn token 発行や vault tenant 正規化は、個人用 proxy では守る相手がいないので不要。harnessrouter 自身も self-host では `owner` trust で broker を外している。

### 2.3 失敗の「code」と retry 可否をクライアント契約として明文化する

- harnessrouter (事実): 1.5 のとおり。type (status 由来) と code (具体的な原因) を分け、code ごとの retry 可否表を仕様に置く。429 でも `rate_limited` (待てば直る) と `quota_exhausted` (待っても直らない) を code で区別。
- llm-gateway の現状 (事実): `error_response` は `Error` の variant を status と Anthropic 形の `error.type` (`not_found_error` / `api_error` / `authentication_error` / `invalid_request_error`) に写し、message に原因を全部書く (個人用なので隠さない、とコメント)。upstream に断られた応答は DR-0009 により**最後の応答をそのまま透過**する (retry-after・リセット時刻を保つため)。
- 評価: 受け口が Anthropic Messages / OpenAI Responses の互換である以上、エラー本文の形はその互換先に従うのが正しく、UHP 形 envelope を持ち込むのは筋が違う。取り込めるのは **gateway 自身が作るエラーに機械可読の code を足す**ことだけ。例えば `UnknownModel` と `UnsupportedRequestShape` は同じ 404 `not_found_error` になっていて、クライアント (やログを読む人) は message の英文でしか区別できない。互換を壊さない追加フィールド (例: `error.code`) なら入れられる。ただし llm-gateway のクライアントは Claude Code / codex で、彼らはその code を読まないので、読み手はログと人間だけ。効果は限定的で、推しは弱い。
- 取り込まない方がよい点: envelope 全体・`detail` の deprecated alias 併記は、クライアントを自分で選べない公開 API の事情。

### 2.4 「拒否されたら次の経路へ行くか」の判断軸 — 逆の結論に至っている理由

- harnessrouter (事実): provider が key を拒否 (401/403/429/quota) したら **次の接続へ行かず失敗させる**。理由は「設定の問題を別 key で隠すと、ユーザはこの key が動いていると思い込む」。一時エラーは次へ。
- llm-gateway の現状 (事実): DR-0009 は 401/403/429/529 を「この経路に断られた」として**次の経路へ行く**。全滅時は最後の応答を透過。締め出し期間と範囲は断られ方で変える。外した理由は events の `skipped` と usage の `denials` に出す (DR-0020)。
- 評価: 前提が違うので、llm-gateway が DR-0009 を変える理由にはならない。harnessrouter の chain は「顧客の key → 共有プール」のように**課金主体が違う接続**が並びうるので、黙った fallback は請求先のすり替えになる。llm-gateway の経路はすべて本人の credential で、429 を別アカウントで逃がすことこそが目的。一方で harnessrouter の懸念のうち「key が壊れていることに気づけない」は llm-gateway にも当てはまり、DR-0009 は 401/403 (429 窓・組織拒否以外) に**締め出しの印を付けない**ので、失効した credential が毎回先頭で 401 を返して次へ回る状態は、遅延が乗るだけで表面上は動いて見える。DR-0020 の `denials` / events `skipped` で見えるようにはなっているが、**気づかせる (通知する) 経路**があるかは確認していない (5 節)。取り込むなら「401 が続く credential を status / usage で警告として目立たせる」程度で、fallback の挙動は変えない方がよい。

### 2.5 400 の本文から「受けないフィールド」を読んで送り直す

- harnessrouter (事実): Google の 400 本文から未知フィールド名を正規表現で抜き、除去して最大 4 回送り直す。除去は学習せずリクエストごと。
- llm-gateway の現状 (事実): DR-0003 で upstream が拒否する beta フラグを credential 単位で**学習**する。
- 評価: llm-gateway の方が一段進んでいる (学習して 2 回目以降の往復を消す)。参考になるのは「送り直しの回数上限を明示する」点と、「エラー応答本文を aread した後に `content-encoding` ヘッダを付けたまま返すとクライアントが復号に失敗する」という実害の記録 (`llm_broker` 内コメント)。llm-gateway が 400 を読んでから返す経路 (DR-0003 の学習時) で同じことをしていないかは確認していない (5 節)。

### 2.6 cancel を一方向ラッチにする / 総時間でなく無活動で timeout する

- harnessrouter (事実): 1.6 のとおり。終状態ラッチと、仕様で「streaming は無活動 timeout + 30 秒ごとの keep-alive コメント」。
- llm-gateway の現状: クライアント切断時の upstream 側の扱いと、転送の timeout の種類 (総時間か無活動か) を今回の範囲では確認していない (5 節)。
- 評価: llm-gateway は 1 リクエスト = 1 転送のパススルーで、task のような長寿命の状態を持たないので、ラッチ自体は不要。無活動 timeout の考え方は、長い thinking を含む SSE 転送に総時間 timeout が掛かっていないかの点検項目として使える。

## 3. ccmsg が参考にできる点

### 3.1 契約の版の交渉と discovery

- harnessrouter (事実): 全応答に `UHP-Version`、非対応版の要求は 400 + `supported` 一覧、無認証の `GET /v1/uhp` が versions と capabilities (全キー必須、false も明示) を返す。版の互換規則は `protocol/VERSIONING.md` に独立。
- ccmsg の現状 (事実): 契約 `@ccmsg/protocol` を版で pin し、新系統は別 instance として立てる (README「v1 を並走させない」)。peers の行には接続ごとに `client_version` / `protocol_version` が載る (`docs/DESIGN.md` の peers 節)。
- 評価: ccmsg は「並走させず別 instance」を選んでいるので、1 サーバで複数版を捌く交渉は不要。取り込めるとすれば **capabilities の「省略と false を区別する」規則** (省略は旧サーバと見分けがつかないので全キー必須) で、契約に capability 相当の宣言を足す時の書き方として有用。現時点で ccmsg に capability 宣言があるかは確認していない。

### 3.2 fail-closed / best-effort を呼び出し側が選ぶ

- harnessrouter (事実): control store の docstring。
- ccmsg の現状 (事実): DR-0008「inbox は永続、drop は配送済みにしない」、DR-0010「超過は突き返す」など、個別には失敗時の倒し方を決めている。
- 評価: 新しい知見というより既存方針の追認。該当なしに近い。

### 3.3 それ以外

該当なし: ccmsg は人の認証を passkey + opaque token + family rotation (DESIGN の認証節) で持ち、harnessrouter の API key / HMAC token より設計が細かい。LLM credential の中継も持たない。

## 4. 参考にしない方がよい点と理由

- **15,892 行の単一ファイル**: routing・認証・変換・永続化・MCP・共有 URL が 1 モジュールに同居し、責務境界がコードに現れない。DR-0014 の三境界 (ingress/egress/exchange) と「core は provider の名前を知らない」とは真逆。
- **model 名の部分一致による backend 推定** (`_route_backend`): `"gpt"` を含めば codex 等。新しいモデル名で誤判定しうるうえ、該当なしは既定 backend に黙って落ちる。llm-gateway の明示的な routing 設定の方が正しい。
- **provider 固有の分岐が core に直書き** (`if provider == "google"` 等が broker 本体に散在、model ID 表が provider ごとの dict)。DR-0014 の provider trait 束に対する反例。
- **失敗判定をエラー文の正規表現で行う** (`_provider_refused` がエラー 1 行目の英文に `401|quota|forbidden` 等をマッチ)。runner が CLI の出力を文字列で返すため仕方ない面はあるが、llm-gateway は status を直接持っているので真似る理由がない。
- **HMAC 鍵の既定値 `"dev-insecure"`** (`INTERNAL_KEY or "dev-insecure"`): 未設定で起動すると推測可能な鍵で token が作れる。fail-closed を謳う節の中にある fail-open。
- **policy 文書の 2 形式を両方読む互換** (`_policy_chain`): 公開済みドキュメントとの互換のためで、個人ツールの llm-gateway が持つ理由はない。

## 5. 未確認・要裏取りの点

- harnessrouter のコードは読んだだけで実行していない。broker の path allowlist の中身 (`_broker_path_allowed`、`HR_BROKER_PATHS` の既定値) は未読。
- harnessrouter の failed response が code (`provider_error` 等) を実際にどう付け分けているか (`_RespTranslator` 内で見えたのは `type=harness_error, code=turn_failed` の 1 箇所)。仕様の code 表と実装の対応は未検証。
- llm-gateway で 401/403 が続く credential を人に気づかせる通知経路 (DR-0020 の表示以外) があるか (2.4)。
- llm-gateway が DR-0003 の学習で 400 本文を読んだ後、`content-encoding` を付けたまま返す経路があるか (2.5)。
- llm-gateway の転送 timeout が総時間か無活動か、クライアント切断時に upstream 接続をどう畳むか (2.6)。
- DR-0030 の「未確定」節で ns 認証方式 (JWT 等) がどこまで決まっているか (2.2 の比較材料として)。本調査では Decision §1–4 冒頭までしか読んでいない。
- ccmsg の契約に capability 宣言が存在するか (3.1)。
- ccmsg 側の実コード照合 (2026-09-24): ccmsg 統括が本ファイルの ccmsg 向け所見を実コードと突き合わせた評価を、ccmsg リポの docs/issue/2026-09-24-harnessrouter-research-review-for-ccmsg.md に記録している。前提がずれていた所見と、本研究が拾えていなかった取り込み候補 (transcript の stop_reason の max_tokens / refusal を ccmsg が表示していない件) はそちらが正本。
