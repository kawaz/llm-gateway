# HarnessRouter の Unified Harness Protocol (UHP) 調査 — llm-gateway / ccmsg への示唆

- 日付: 2026-09-23
- 対象: HarnessRouter/harnessrouter リポの `protocol/` 配下 (現行版 `2026-09-12`)
- 比較: kawaz/llm-gateway、kawaz/ccmsg (契約は kawaz/ccmsg-protocol)
- 性質: 読み取り調査。実機での疎通・conformance suite の実行はしていない

## 1. 対象の要約 (事実)

### 1.1 位置づけ

- UHP は「agent harness (Codex / Claude Code / Hermes 等) を product から駆動する」ための HTTP 契約。model API ではなく、単位は *turn* でなく *task* (`protocol/README.md`「UHP is not a model API」)
- 役割は Client → Server → Harness の 3 者。Server が harness をどう動かすか (container / subprocess / queue) は仕様外で、wire に漏らしてはならない (`versions/2026-09-12/architecture.md` §1)
- 「configured harness」= base (`codex` 等) + 設定 (既定モデル・system prompt・tool 制限・skills・MCP・step/time 予算) を id (`chrn_`) で指せる第一級オブジェクトにしている (同 §1)
- 設計原則 6 つ (同 §6): harness の違いを client に見せない / 進捗は描画でなく事実の stream / 1 フィールド 1 意味 (借りた面は足すだけで再定義しない) / **不明な値は省く、0 や推定で埋めない** / 失敗は成功と同じ封筒で機械可読 code を持つ / 仕様・参照実装・conformance suite は一緒に動く

### 1.2 オブジェクトとライフサイクル

- オブジェクトは 6 種 (harness `chrn_` / response `resp_` / session `hsess` / file `file_` / container `cntr_` / event)。全部が `object` フィールドで型を名乗り、id の prefix で型が分かる (`architecture.md` §3)
- session は暗黙に作られる。最初の task で生まれ、`previous_response_id` で延長される。理由は「`POST /sessions` を必須にすると往復・失敗モード・後始末対象が 1 つ増える」(同 §3 の Why)。連鎖を session id でなく response id で張る理由は「client が既に持っている値」かつ「会話の特定点を指すので将来の分岐に余地を残す」(`sessions.md` §1)
- task の状態は `in_progress` → `completed` / `failed` / `incomplete` / `cancelled` (`lifecycle.md` §3)。終端から出ない。`incomplete` は予算 (step/time) で止まった場合専用でエラーには使わない。`cancelled` を `failed` にしない。終端後も途中までの出力を保持する
- 同一 session での並行 task は禁止で `409 session_busy`。`retry_after_ms` は見積もれる時だけ入れ、見積もれないなら省く (同 §5)
- 別 harness で session を延長しようとしたら黙って新 session を始めず `409 harness_mismatch` (同 §4)
- cancel は 2 スコープ (`/responses/{id}/cancel` と `/sessions/{id}/cancel`)。冪等 (終端済みへの cancel は成功して何も変えない)、session は消さない、1 秒以内に応答すべき (`sessions.md` §4)。削除 (`DELETE /responses/{id}`) は実行中 task を cancel してはならない — cancel と delete は別の意図 (`tasks.md` §7)。ただし session 削除だけは in-flight task を先に cancel する (`sessions.md` §6)

### 1.3 Responses 互換の取り方

- task 面は OpenAI Responses API の形に意図的に合わせ、`POST /v1/responses` の部分集合を受ける義務がある (`README.md`「Relationship to the OpenAI Responses API」)
- 拡張は `metadata`・少数の追加フィールド・追加オブジェクト型に限り、既存フィールドの意味は変えない。harness の指定も top-level ではなく `metadata.harness_id` (`tasks.md` §1.2 の Why:「top-level に置くと既存 SDK を全部直すことになる」)
- 未知フィールドは拒否せず無視。ただし **無視は観測可能でなければならない**: 行動しなかったフィールド名を `metadata.ignored_fields` に列挙する (`tasks.md` §1.1)
- `tools` / `include` は「reserved and ignored」と明示決定。`tools` は harness が自前で tool を実行するモデルなので戻り足 (function_call_output の入力経路) が無い、request 単位の MCP 付与は権限昇格になる (「narrowing is safe, widening is escalation」)、`include` は語彙が無い (`tasks.md` §1.4)
- モデルが出せないときは「422 `model_unavailable` で落とす」か「既定に差し替えて `metadata.requested_model` / `model_fallback` / `model_fallback_reason` を載せる」のどちらか。黙った差し替えは禁止 (`tasks.md` §1.3)
- `usage` は計上できないなら `null`。0 を捏造しない (`tasks.md` §3)
- 冪等性: `Idempotency-Key` を持つ再送は最初の結果を返し、2 回目の実行をしない。最初が走行中なら待って結果を返す。キー保持は 24h 以上推奨 (`tasks.md` §6)。`errors.md` §4 は「`POST /v1/responses` の再送は `Idempotency-Key` 必須」とする

### 1.4 ストリーミング

- SSE。全 event が `type` と `sequence_number` を持ち、0 始まりで 1 ずつ増える (欠番検出のため) (`streaming.md` §1)
- 語彙は Responses のもの (`response.created` / `output_item.added|done` / `output_text.delta` / `reasoning_summary_*` / `function_call_arguments.*` / `error`) (同 §2)
- 順序保証は「created が最初」「終端 event がちょうど 1 つで最後」「item の added が先、done が後」「sequence_number が単調で欠番なし」の 4 つだけ。item の完了順・チャンク粒度・任意 event の出現は保証しない (同 §3)
- 終端 event は完全な最終 `response` を運ぶ (途中を取りこぼしても終端 1 つで結果が組める)。cancel は `response.failed` 名で終わるが `status: "cancelled"` が正本 (同 §4)
- `error` event の後には必ず終端 event を出す (task 死亡と接続断を区別させるため) (同 §2.6)
- 接続が切れても task は止めない。追跡は `GET /v1/responses/{id}` の再読込が基本で、`Last-Event-ID` 再開は MAY (同 §5)。「stream は最適化、保存された response が正本」
- 非 stream と stream の結果は同一でなければならない (同 §6)
- stream には総時間 timeout を掛けず無活動 timeout を使う。server は 30 秒以内ごとに `: keep-alive` コメント行を出すべき (`errors.md` §5)
- proxy のバッファリングが最頻出の配備ミスとして名指しされ、conformance の S-09 が event 到着時刻の広がりを測って検出する (`conformance/README.md`)

### 1.5 エラー

- 非 2xx は全て `{"error":{type, code, message, param, detail}}` の封筒 (`errors.md` §1)。`message` は利用者に見せて安全 (credential・内部ホスト名・パス・stack trace を含めない)
- 「request の失敗」(非 2xx) と「task の失敗」(HTTP 200 + `status: failed`) を混同しない (同 §1)
- `type` は 6 分類 (`invalid_request_error` / `authentication_error` / `permission_error` / `rate_limit_error` / `harness_error` / `server_error`)、`code` は閉じた一覧で、独自 code は vendor prefix 必須 (同 §2, §3)
- 再試行表: server_error はバックオフ付き再試行、`rate_limited` は `Retry-After` 後、`quota_exhausted` は再試行しない、`session_busy` は終端待ち後 (同 §4)
- `rate_limited` (待てば通る) と `quota_exhausted` (待っても通らない) を別 code にしている (同 §3.2)
- scope 外オブジェクトには 403 でなく 404 を返して存在を漏らさない (`architecture.md` §5)

### 1.6 版管理・conformance・運営

- 版は日付 (`YYYY-MM-DD`)。SemVer の約束は守られないことが多いので採らない (`VERSIONING.md`)
- `UHP-Version` request header で版を宣言でき、未対応なら `400 unsupported_protocol_version` で対応版一覧を返す。別の版で黙って応じない。全応答が使った版を header で返す (`lifecycle.md` §1)
- 版内で許すのは任意フィールド追加・応答フィールド追加・event 型追加・prefix 付き code 追加・制約緩和。削除・改名・型/意味変更・必須化・制約強化は新版 (`VERSIONING.md`)。client 側 4 規則 (未知フィールド・未知 event・未知 item 型を無視、未知 code は `type` として扱う)
- `GET /v1/uhp` を無認証で出す discovery 文書。capability は未実装を省かず `false` で明示 (「未対応」と「古い server」を区別するため)、`conformance_class` と capabilities の整合を suite が検査 (`lifecycle.md` §2)
- conformance class は Core / Extended / Full の累積 3 段 (`architecture.md` §2)
- conformance suite (`conformance/`、Python) を仕様の一部とし「suite に通ること」だけが conformant の定義。実 agent task を約 6 本走らせる (実トークンを使う) — flush しない stream・止まらない cancel・落とせない artifact は schema 検査では見えないから。75 checks。PASS/FAIL/SKIP/ERROR を分け、**SKIP は PASS ではない** (`conformant` は skip 1 つで false、`conformant_with_skips` と `skipped_not_verified` を別に出す) (`conformance/README.md`)
- suite 自身のテストとして「わざと壊した stub server」に対して各 check が落ちることを確かめている (`conformance/tests/share_defect_stub.py` 等、`test_session_sharing_checks.py`)
- 変更は UEP (issue) → 仕様・schema・参照実装・conformance test・CHANGELOG を 1 PR で (`GOVERNANCE.md`)。「declined field は pending ではない」— 実装しないと決めたフィールドは決定として 1 度書き、呼び手には `ignored_fields` で伝える (同)
- 既知の妥協を仕様内に列挙している (snake_case と camelCase の混在、`/v1/traces/{id}` の旧パス) (`VERSIONING.md`「known compromises」)

## 2. llm-gateway が参考にできる点

### 2.1 events の欠番検出 (`sequence_number`)

- **harnessrouter (事実)**: stream 内の全 event に 0 始まり・1 刻みの `sequence_number` を振り、client が取りこぼしを検出できる (`streaming.md` §1, §3)
- **llm-gateway の現状 (事実)**: `/llm-gateway/events` は `tokio::sync::broadcast` で配り、溜めは 256 件、追いつけない見物人の分は落とし、落とした数は gateway 側のログにだけ残す。過去には遡らない (DR-0012「見ている人がいなければ何もしない。遅れたら落とす」)。webhook も「再送と順序保証は行わない」(DR-0012 webhook 節)。event に通し番号は無い (DR-0012 の欄一覧に該当欄なし)
- **取り込むなら (評価)**: 落とす方針は変えずに、gateway プロセス単位の単調番号 (と起動ごとの epoch) を 1 欄足すだけで、見る側 (ccmsg の WebUI / `llm.requests` topic) が「落ちた」を知れる。取りこぼしを知った側は `cache_*` の見込みを信用しない / `/llm-gateway/usage` 等を取り直す、といった判断ができる。DR-0012 の「転送を見物人の都合で遅らせない」を侵さない (番号付けは送る側だけで完結する)。DR-0024 系の cache 窓表示は「最新の知らせで上書き」なので、欠落を知る価値は `request` と `response` / `cache_expired` の対応が崩れる場面で効く
- **取り込まない方がよい理由**: UHP の番号は「1 stream = 1 task」の中の話で、gateway の events は全転送が混ざる broadcast。webhook は複数受け口・バッチで送るので、受け口ごとの欠番と gateway 全体の番号は一致しない (同じ番号体系で両方を語ると誤読を生む)。入れるなら「購読接続ごと」か「gateway 全体」かを先に決める必要がある

### 2.2 「不明は省く、0 を捏造しない」を仕様として明文化

- **harnessrouter (事実)**: 原則 4「Absent is not empty, and empty is not zero」、`usage` は計上不能なら `null`、`retry_after_ms` は見積もれなければ省く (`architecture.md` §6、`tasks.md` §3、`lifecycle.md` §5)
- **llm-gateway の現状 (事実)**: 個別には既に同じ姿勢。DR-0012 の `cache_*` は「繋ぐ対象の 1 本にだけ出る。そうでなければ欄ごと出さない」「単価が分からず分岐時間を出せないモデルでは欄ごと出さない」、DR-0025 §7 は codex の `ModelInfo` を組み立てず運ぶ理由を「gateway が知りようのない値まで捏造することになる」としている
- **取り込むなら (評価)**: 新しい仕組みは要らない。欄ごとに毎回理由を書いている姿勢を、DR か MANUAL の出力規約として 1 箇所に上げれば、今後の欄追加で都度判断しなくて済む。推しは「規約化はしてよいが優先度は低い」
- **取り込まない方がよい理由**: 特になし (懸念点なし)

### 2.3 黙った差し替えを応答に書く (`requested_model` / `ignored_fields`)

- **harnessrouter (事実)**: モデル差し替え時は `metadata.requested_model` / `model_fallback` / `model_fallback_reason` を載せる。行動しなかった request フィールドは `metadata.ignored_fields` に列挙する (`tasks.md` §1.1, §1.3)
- **llm-gateway の現状 (事実)**: model は namespace の alias で解決される (MANUAL-ja.md の `[ns.*.aliases]`、DR-0025 §1「model 欄の alias 解決以外は本文に触らず」)。経路の fallback (DR-0009) と外した理由は events の `skipped` と usage の `denials` に出る (DR-0020)。本文を書き換える操作として DR-0016 の thinking.display 強制上書き、DR-0024 の cache 戦略 (`cache_control` の付与) がある。これらを **クライアントへの応答** の中で知らせる仕組みは、今回読んだ DR・コードの範囲では見当たらない (観測は events / tap 側)
- **取り込むなら (評価)**: gateway は透過 proxy で応答本文を Anthropic / OpenAI の形のまま返すので、UHP のように応答 body の `metadata` に書くのは形を壊す。代わりに応答 header (例: 解決後の model、上書きした項目名) で出す形なら透過性を保てる。ただし読むクライアント (Claude Code / codex CLI) は header を読まないので、効くのは人と events 側だけ。既に events が同じ役を持つので、追加価値は「events を購読していない時の curl デバッグ」程度
- **取り込まない方がよい理由**: クライアントは変更不能な既製品で、UHP の「client に見せる」前提が成り立たない。可視化の置き場は events (DR-0012 / DR-0020) で既に決まっており、2 箇所目を作ると同じ事実の出口が 2 つになる。推しは「取り込まない。events 側の欄で足りているか確認するに留める」

### 2.4 エラー封筒と `rate_limited` / `quota_exhausted` の区別

- **harnessrouter (事実)**: `type` × `code` の 2 層、閉じた code 一覧、再試行可否の表。待てば通る `rate_limited` と待っても通らない `quota_exhausted` を分ける (`errors.md`)
- **llm-gateway の現状 (事実)**: gateway 自身が作るエラーは `crates/llm-gateway-server/src/lib.rs` の `error_response` / `refused` で、Anthropic 形式 `{"type":"error","error":{"type":…,"message":…}}` を返す。`AllUpstreamsFailed` / `UpstreamUnreachable` は 503 `api_error`、message には全経路の試行詳細を載せる (「個人利用の proxy なので、隠すより原因が分かるほうが役に立つ」とコメント)。上流のエラーは DR-0009 で生透過、本文内 error は DR-0014 §9 で status に写像 (overloaded → 529、rate limit → 429、入力起因 → 400、判別不能 → 502)。全滅時の 429 の置き場は DR-0014 §8
- **取り込むなら (評価)**: code の閉じた一覧は、クライアントが既製品 (Anthropic / OpenAI 形を読む) なので wire には持ち込めない。一方「全経路が枠切れ (待てばリセットで戻る) か、経路が壊れている (待っても戻らない)」の区別は gateway の全滅応答で有用で、`retry-after` を DR-0015/0018 のリセット時刻から出せるなら出す、出せないなら省く、という UHP §5 の流儀は合う。今の全滅 503 がこれを区別しているかは未確認 (§5 参照)
- **取り込まない方がよい理由**: UHP の `message` 秘匿規則 (内部ホスト名・パスを出さない) は多主体サービス向け。gateway は個人用で、コードも「隠すより原因」を明示的に選んでいる。この点は取り込まない方がよい

### 2.5 Responses 互換の範囲の決め方 (declined を決定として書く)

- **harnessrouter (事実)**: Responses の形を借りつつ、意味を持てない `tools` / `include` を「reserved and ignored」と明示決定し、理由を仕様に書く。GOVERNANCE の「A declined field is not a pending one」(`tasks.md` §1.4、`GOVERNANCE.md`)
- **llm-gateway の現状 (事実)**: DR-0025 は Responses を無変換パススルーにし、`store` / `include` / `tools` 等はクライアントが送ったまま (§1)。Responses → Messages 変換は「この DR の範囲外、要求が出た時点で別 DR」(§1)。cache 戦略は当てない (§5)
- **取り込むなら (評価)**: パススルーなので「どのフィールドを扱わないか」の問題は今は無い。効くのは将来 Responses → Messages 変換 (Anthropic 上流へ出す) を作る時で、その時は「変換できないフィールドは拒否せず落とし、落としたことを events に書く」という UHP の型が素直に使える。今やることは無い
- **取り込まない方がよい理由**: 現時点では該当なし (パススルーでは扱わない欄が生じない)

### 2.6 版 header と discovery

- **harnessrouter (事実)**: `UHP-Version` header の交渉、無認証の `GET /v1/uhp` に capability を `false` 込みで列挙 (`lifecycle.md` §1, §2)
- **llm-gateway の現状 (事実)**: 管理口は `/llm-gateway/` 配下 (DR-0006)。events / usage / stats / status の JSON に版を表す欄を持つかは、今回読んだ DR の範囲では記述なし。主な消費者は ccmsg (DR-0012 webhook、ccmsg DR-0012)
- **取り込むなら (評価)**: 消費者が ccmsg だけで、両方 kawaz が同時に直せるので、版交渉の仕組みは過剰。ccmsg-protocol DR-0017 の「世代 1 つ、互換経路を持たない」の方が状況に合う。やるとしても events の JSON に `schema` 世代番号を 1 つ載せる程度
- **取り込まない方がよい理由**: 第三者 client が居ない。日付版と多版同時提供は、独立実装が複数ある標準のための道具

### 2.7 実物を走らせる conformance と「わざと壊した stub」

- **harnessrouter (事実)**: suite は実 agent task を走らせ、buffering (S-09)・止まらない cancel (C-03) のような schema で見えない欠陥を測る。check 自身を「欠陥を 1 つずつ仕込んだ stub server」で落ちることを確かめる (`conformance/README.md`、`conformance/tests/*_stub.py`)
- **llm-gateway の現状 (事実)**: DR-0025 の受け入れ確認は実機 1 回 (codex CLI 0.153.4 で `codex exec` が通り tap が記録) を DR に記録。自動テストは crate 内の単体・結合テスト (`crates/llm-gateway-server/src/lib.rs` の tests)
- **取り込むなら (評価)**: 特に「stream が逐次 flush されているか」は proxy である gateway の核心の性質で、UHP S-09 の測り方 (event 到着時刻の広がり) は mock upstream を相手に結合テストとして移植しやすい。DR-0014 §9 の「採用前判定で本文先頭を見る」区間が、意図せず flush を遅らせていないかの回帰検知にもなる。推しはこれが本調査で一番安く効く取り込み
- **取り込まない方がよい理由**: 実トークンを使う suite は個人用 proxy の CI には重い。移植するなら mock upstream 相手の測定に限る
- **取り込み済み**: `crates/llm-gateway/src/gateway.rs` の `a_messages_stream_is_flushed_chunk_by_chunk` / `a_responses_stream_is_flushed_chunk_by_chunk` / `a_kept_main_stream_is_flushed_chunk_by_chunk`。到着時刻の広がりでなく、upstream が「前の chunk をクライアントが受け取った」合図を待って次を送る lockstep で測る (負荷で到着が詰まっても誤判定しない)

### 2.8 SSE keep-alive 間隔

- **harnessrouter (事実)**: server は 30 秒以内ごとに `: keep-alive` を送るべき (`errors.md` §5)
- **llm-gateway の現状 (事実)**: events は 20 秒ごとに SSE コメント行 (DR-0012「keepalive は 20 秒」)。転送中の本流 stream は上流のものを透過
- **評価**: 既に満たしている。取り込むものなし

## 3. ccmsg が参考にできる点

### 3.1 状態機械の終端規則 (`incomplete` と `cancelled` を `failed` にしない)

- **harnessrouter (事実)**: 終端 4 種を区別し、予算停止は `incomplete`、利用者の中止は `cancelled`、終端から出ない、終端後も途中出力を保持 (`lifecycle.md` §3)
- **ccmsg の現状 (事実)**: session と run を分け、`liveness` / `reachable` / `waiting` は契約側の関数で読む (DESIGN §4、ccmsg-protocol DR-0001)。`session_status` は `absent` / `folding` / `ready` / `frozen` (DESIGN §6.2)。busy の正本は gateway (DR-0009)
- **取り込むなら (評価)**: ccmsg は harness を「駆動」せず「観測」する立場なので、task 状態機械そのものは持ち込めない。ただし「なぜ止まったか」(利用者の中止 / 予算 / 失敗) を区別して見せる必要が webui に出てきたら、UHP の 4 分類は語彙の候補になる。今の transcript から止まった理由を読めるかは未確認
- **取り込まない方がよい理由**: ccmsg の DESIGN §1 は「upstream の判断を再導出しない」を掲げている。harness が明示しない終端理由を ccmsg が推定して付けるのはこれに反する

### 3.2 `retry_after_ms` は見積もれる時だけ・`rate_limited` と `quota_exhausted` の分離

- **harnessrouter (事実)**: `session_busy` に見積もれる時だけ `retry_after_ms`、再試行可否を code で分ける (`lifecycle.md` §5、`errors.md` §3.2, §4)
- **ccmsg の現状 (事実)**: `rate_limited` は「引数は正しく何も失敗していない、読み手が追いついたら同じ呼び出しが通る」(ccmsg-protocol `src/errors.ts`、ccmsg DESIGN §6.4)。`internal_error` は「再試行が効くかは述べない」(同 `errors.ts`)。閉じた 21 code の union で、op ごとに返しうる code を `opErrors` が持つ (ccmsg-protocol README)
- **評価**: 閉じた一覧・op 単位の code 宣言は UHP より既に厳密 (UHP は vendor prefix で開いている)。取り込めるのは「送出キューが 100ms flush で空く見込みを `retry_after_ms` として返す」程度だが、ccmsg の `rate_limited` は `QUEUE_LIMIT` 超過で、待ち時間は flush 周期から概算できる。価値は小さい。推しは取り込まない

### 3.3 再接続時の「stream は最適化、保存物が正本」

- **harnessrouter (事実)**: 接続断でも task は続き、client は `GET /v1/responses/{id}` で正本を読み直す。終端 event は完全な最終 response を運ぶので途中の取りこぼしは遅延にしかならない (`streaming.md` §4, §5)
- **ccmsg の現状 (事実)**: 購読の直後に snapshot が同型で届く (ccmsg-protocol DR-0005)。`transcript:<sid>` は byte offset を運び、欠落は `start` / `size` で見えて `transcript.read` で読み戻す (DESIGN §6.2, §6.4)。購読は接続に従属し、切れたら消える (DESIGN §6.3)
- **評価**: 同じ思想を snapshot + delta で既に実現しており、UHP より一般化されている (全 topic に適用)。取り込むものなし

### 3.4 conformance の「SKIP は PASS ではない」と欠陥 stub

- **harnessrouter (事実)**: SKIP を PASS と分けて報告し、skip 1 つで `conformant: false`。check を欠陥 stub で落として check 自体の効き目を確かめる (`conformance/README.md`)
- **ccmsg の現状 (事実)**: 契約が実行可能 (schema + validator)、fixtures を契約が持ち実装テストがそれを読む (ccmsg-protocol DR-0024)。DESIGN §9.2「認可境界は常に直接テストする」、§9.3「do not grow を壊す変更を検出する」
- **取り込むなら (評価)**: fixtures は「正しい形」の代表例。UHP の stub のような「壊れた実装」側の資料は持っていない。daemon と webui が同じ validator に照らす構造なので、validator が不正な frame を落とすことの負例 fixture (壊れた JSON 群) を契約が持つ、という形なら自然に足せる。推しは中程度
- **取り込まない方がよい理由**: 実装が daemon と webui の 2 つで両方 kawaz 製。独立実装が複数ある UHP ほど「suite が権威」の構造は要らない

### 3.5 cancel の冪等性と 1 秒以内の応答

- **harnessrouter (事実)**: 終端済みへの cancel は成功扱いで何も変えない、応答は 1 秒以内 (`sessions.md` §4)
- **ccmsg の現状**: harness への割り込み・停止に当たる op が契約にあるか、今回の読み取り範囲では確認していない (§5)
- **評価**: 該当 op があるなら「二重送信を成功として扱う」は再送時の誤表示を防ぐ規則として有用。未確認のため判断保留

## 4. 参考にしない方がよい点と理由

- **日付版・多版同時提供・6 か月の旧版維持** (`VERSIONING.md`): 独立実装が多数ある公開標準のための仕組み。llm-gateway の管理口と ccmsg の契約は消費者が kawaz 製で同時に追従でき、ccmsg-protocol DR-0017 は「世代 1 つ、互換経路を持たない」を理由付きで選んでいる (案 B「変換層は契約が否定した同一性を実装が主張する」)。UHP の方式を入れると DR-0017 が退けた組み合わせ分岐を持ち込む
- **conformance class (Core / Extended / Full) の段階化**: 実装者が機能を選んで名乗るための区分。ccmsg は capability を op 属性表の `capability` で既に持ち (ccmsg-protocol DR-0004、`capability_unavailable`)、class という第 2 の粒度は不要
- **scope 外に 404 を返し存在を隠す、message から内部情報を消す** (`architecture.md` §5、`errors.md` §1): 多主体 SaaS 前提。llm-gateway は tailnet 境界を信頼し管理口に認証を掛けない (DR-0012、DR-0006) 個人用で、コードは意図的に原因を全部書く (`crates/llm-gateway-server/src/lib.rs` の `error_response`)。取り込むと診断性を失うだけ。ただし DR-0030 (汎用認証 gateway、提案中) が多主体 (JWT 認証・ns allowlist) に踏み込むなら、その時点で再評価の対象
- **Idempotency-Key で「走行中の最初を待って同じ結果を返す」**: UHP の task は副作用 (ファイル編集) を持つ長時間仕事なので価値がある。llm-gateway は LLM 呼び出しの透過で、クライアント (Claude Code / codex CLI) は key を送らない。gateway 側で重複検出して応答を複製するのは、stream の複製・保持が要り DR-0012 の「状態を持たない」から外れる
- **Responses を「受ける面」として標準化する発想そのもの**: UHP は Responses の形を *自分の API の形* として借りる (task 面)。llm-gateway の DR-0025 は Responses を *上流と同じ方言として素通しする* 立場で、形を所有していない。UHP の `ignored_fields` や `metadata.*` 拡張をパススルー上で真似ると、上流が知らない欄を本文に足すことになり DR-0025 §1 の無変換原則を壊す
- **harness を駆動する (task を投げる) 役割**: ccmsg は session 間メッセージングと観測で、DESIGN §1 と README「What it does not do」で upstream の判断を再導出しない・自前の会話ログを持たないとしている。UHP の task / session / container モデルは ccmsg のスコープ外

## 5. 未確認・要裏取りの点

- llm-gateway の Responses 受け口 (`POST /v1/responses`) で gateway 自身が断る時 (例: 運べる経路が無い 404、全滅 503) も、`refused` の Anthropic 形 `{"type":"error","error":{…}}` を返していると読める (`crates/llm-gateway-server/src/lib.rs` の `error_response` は受けた形で分岐していない)。codex CLI がこの形を読めて原因を表示するかは未確認。OpenAI 形は `{"error":{"type","code","message","param"}}` で top-level `type` を持たない。実機で codex CLI に全滅応答を返して表示を見る必要がある
- llm-gateway の全滅応答 (503 `api_error`) が「全経路が枠切れ (待てば戻る)」と「経路が壊れている」を区別しているか、`retry-after` を付けているかは DR-0014 §8 (全滅時 429 の置き場) を本文まで読んでいないため未確認
- llm-gateway の events / usage / stats の JSON に世代や版を示す欄があるかは、コードを読んでいない
- ccmsg / ccmsg-protocol に harness への割り込み (中止) に当たる op があるか、その冪等性は未確認
- UHP の `GET /v1/harnesses/{harness_id}/events` (`streaming.md` §5 で言及) は endpoint 一覧 (`versions/2026-09-12/index.md`) に載っておらず、仕様内で定義箇所を確認できなかった。仕様側の記述漏れか、別章 (harnesses.md) にあるかは未確認
- UHP の `plugins.md` / `files.md` / `security.md` / `harnesses.md` / `schema/` / `CHANGELOG.md` は本調査の観点 (ライフサイクル・stream・エラー・版) から外れるため精読していない
- conformance suite は実行していない。「75 checks」「S-09 の測り方」は README の記述に基づく
