# harnessrouter 研究: UI と可観測性

調査日: 2026-09-23。対象は HarnessRouter/harnessrouter (main)。比較対象は kawaz/ccmsg、kawaz/ccmsg-webui、kawaz/llm-gateway。読み取り専用の調査で、実機での起動・計測はしていない (§5)。

## 1. 対象の要約 (事実)

### 1.1 構成と技術選定

- webui は `ui/` の Next.js (App Router) アプリ。`ui/package.json` の依存は tiptap + hocuspocus (共同編集)、`@xyflow/react`、cytoscape、echarts と chart.js の 2 つの図 library、framer-motion、axios、Tailwind (`ui/tailwind.config.ts`) など多数。
- CSS は `ui/src/app/hr.css` (949 行)、`v2.css` (1002 行)、`revamp.css` (2054 行)、`hr-billing.css` と `ui/src/studio/styles/` の 23 ファイルが並ぶ。名前 (`v2` / `revamp`) の通り、改修ごとに層が足されている。
- ブラウザは gateway を直接叩かず BFF (`ui/src/app/api/harness/[...path]/route.ts`) を通す。鍵はブラウザに渡さず、BFF が内部鍵を注入し、ブラウザは org / member / workspace をヘッダで名乗る (`ui/src/lib/chat.ts` 冒頭コメントと `authHeaders()`)。
- gateway は Python 1 ファイル (`gateway/app.py`)、runner は `runner/server.py` と harness ごとの driver。

### 1.2 タスク進捗のストリーミング

- 1 ターンは `POST /v1/responses` (`stream: true`) で、OpenAI Responses 互換の SSE がそのまま返る。フレーミングと event の振り分けは共有 package `reifyui` の `pumpResponsesStream` / `readSSEStream` にある (`ui/src/lib/chat.ts`)。handler は reasoning / tool call / tool result / text delta / file / done / error に分かれる (`StreamHandlers`)。
- それとは別に、harness 単位の broadcast bus `GET /v1/harnesses/{hid}/events` を購読する (`subscribeHarnessEvents`)。「どのタブ / どの POST がターンを始めたかに依らない、live 更新の正本」とコメントにある。再接続は固定の 1.5 秒 / 2 秒待ちで、`Last-Event-ID` 等の再開位置は持たない。
- gateway 側の bus は topic = (org, harness)、購読者ごとの asyncio.Queue、複数 replica は Redis pub/sub で全 replica が同じものを配る (`gateway/app.py` 528-608 行付近)。**実行中ターンの event を session ごとに最大 6000 件保持する replay buffer** (`_turn_buffers`, `_BUF_MAX`) があり、ターン途中で開いた / 再読込したクライアントに進行中ターンを即座に渡す。完了済みターンは replay せず `/v1/sessions/{sid}/turns` から読ませる。
- UI は 2 経路で「ターンが終わった」を確定させる: SSE の終端 event か、4 秒ごとの reconcile poll のどちらか先に来た方 (`ui/src/lib/conversation.ts` 151 行と 385-410 行付近)。コメントは「SSE は best-effort 配送、poll は修復のため」と明言している。自タブの POST が描いているセッションは bus 側を抑止する (`_busSuppress`)。
- 転送の失敗 (POST の SSE が切れた) とエージェントの失敗 (`onDone('failed')`) を分ける。前者は「サーバ側ではまだ走っている」可能性があるので失敗として描かない (`conversation.ts` 555 行付近)。

### 1.3 セッション / タスク / 失敗の見せ方

- ターンの状態語彙は UI で `running` / `done` / `failed` / `cancelled` / `incomplete` の 5 つに畳む。`max_turns` / `timeout` は `incomplete`、ユーザ Stop は `response.failed` + reason `cancelled` を `cancelled` として描く (`conversation.ts` 82-85 行、251-255 行)。
- 失敗理由は assistant 本文の末尾に `Error: <理由>` として積み、HTTP エラーはサーバの `detail` 文をそのまま出す (「transport artifact ではなく actionable な文を見せる」、`chat.ts` の `streamResponse` と `fetchFileBlob`)。
- `SessionTurn` はターン単体の `elapsed` と `credits` を持つ (`chat.ts`)。
- Traces 画面 (`ui/src/studio/traces/`): 一覧は 25 件ページングのカードに status pill と相対時刻 (`TracesSidebar.jsx`)。詳細は **1 event = 1 行のコンパクトな行**で、tool_use と tool_result を id で結び (結果は独立行にせず詳細ペインに入れる)、同じメッセージの並列 tool_use を 1 行にまとめ、init / turn.started 等の足場 event は落とす (`flatten.js` 冒頭コメント)。行の上に**時間比例の帯** (`Strip`) があり、tool 実行は長さを持つ棒、メッセージは細い刻み、20 秒を超える間隙は idle として別色で描く (`TracesMain.jsx`、`IDLE_GAP = 20`)。行ごとに開始からの経過 (H:MM:SS)、所要時間、in / out トークンを出す。
- `/tasks` は廃止されて `/harnesses?h=&sid=` へ redirect する (`ui/src/app/(app)/tasks/page.tsx`)。タスクは harness の中で開く情報設計に寄せられている。

### 1.4 コスト・トークン・レイテンシ

- 課金: gateway がターン終了時に `harness.session_minute` (経過分) と model ごとの input / output 1k 単価から credits を計算する (`gateway/app.py` の `_run_credits`)。単価表の読み込みに失敗したら「そう言う」ことがコメントで要求されている (「さもないと全セッションが 0 credits と表示され、どこも壊れて見えない」、136-140 行付近)。
- 計量の送信は fire-and-forget で、失敗は例外にせず `_OpsDrops` の累積カウンタに数え、一定間隔でログに出し、`/readyz` に載せる (`gateway/app.py` 82-116 行)。bus の取りこぼしも同じ仕組み (`_bus_drops`: subscriber_q / pump_q / sse_q)。
- 利用画面 (`ui/src/app/(app)/billing/usage/page.tsx`): 1 日 / 7 日 / 30 日 / 任意範囲、2 日以下は時間単位・それ以上は日単位の棒、echarts の積み上げ。
- ベンチマーク (`docs/benchmark.md`、`scripts/benchmark/README.md`): 1 タスク = 1 セッション、採点は対象スイート自身の grader。行の列は Tasks / Resolved / Reward / Wall 合計 / Wall 中央値 / Tool calls / 失敗率 / **Fresh in / Cached in / Output** / Served by / Notes。規則は 4 つ: (1) 期待した接続で走った、(2) 要求した model が実際に応答した (`samemodel.py` で同一判定、報告が無い run の数を Notes に書く)、(3) ネットワークや workspace 外の探索をした run は「finding」として採点から外し一覧だけ出す、(4) **fresh input・cached input・output を別々に出して決して足さない**。harness ごとに input の意味が違うので runner の契約で `input_tokens` = fresh に揃え (codex / gemini / cline は差し引いて合わせる)、`output_tokens` は thinking を含む全生成。規則は `scripts/benchmark/test_run.py` が固定している。

### 1.5 通知

- `ui/src/components/revamp/Shell.tsx` のヘッダに通知の入口がある (コメント上)。中身の設計文書は見当たらず、未確認 (§5)。

## 2. ccmsg (webui) が参考にできる点

### 2.1 時間比例の帯 (どこで時間を使ったかの俯瞰)

- harnessrouter (事実): Traces 詳細の上に、event を開始からの時刻で置いた細い帯。tool 実行は所要時間の長さ、20 秒超の間隙は idle として別色。hover で行と連動し、クリックでその行へ飛ぶ (`ui/src/studio/traces/TracesMain.jsx` の `Strip`)。
- ccmsg-webui (事実): Timeline は取得・モデル・描画の 3 層で、呼び出しと答えの結合は `parent_item` / `parent_tool_use_id` で済んでいる (ccmsg-webui `docs/DESIGN-ja.md`「Timeline の 3 層」)。時間軸で俯瞰する表示は DESIGN に無い。
- 取り込むなら (評価): モデル層の `TimelineNode` から (時刻, 所要, 種別) の列を純関数で作り、描画層は `left` / `width` を % で置くだけの CSS で描ける。長いセッションで「待ち」と「道具の実行」のどちらに時間が消えたかが一目で分かる。llm-gateway の `request` / `response` の組 (`request_ts` で 1 対 1) を重ねれば「LLM 応答待ち」も区間として出せる。推し。
- 注意: 画面が持つのは末尾 1 MiB 相当の窓なので、帯が表すのは窓の範囲だけになる。全体の帯が欲しいなら instance 側に集計の op が要り、それは DESIGN の M2 (同じ情報の 2 経路目) に触れるので、窓の範囲の帯と明示するのが無難。

### 2.2 「未完了」を失敗と分ける状態語彙

- harnessrouter (事実): `max_turns` / `timeout` を `failed` ではなく `incomplete` に畳み、ユーザ Stop は `cancelled` として別に描く (`ui/src/lib/conversation.ts`)。
- ccmsg (事実): セッションの見出しは `Duplicated` / `Waiting` / `Live` / `Unreachable` / `Paused` / `Disappeared` で、止まっている理由 (`api_error`) を先頭に出す (ccmsg-webui `docs/DESIGN-ja.md`「セッションと run」「状態は instance が畳んだもの」)。llm-gateway の `response` event は `stop_reason` (`max_tokens` 等) と `aborted` を常に載せる (llm-gateway `docs/MANUAL-ja.md` の `/llm-gateway/events`)。
- 取り込むなら (評価): 「最後の応答が `max_tokens` で切れた」「Esc で切った (`aborted`)」を、失敗でも正常終了でもない第 3 の印として行に出せる。材料は既に gateway が流しているので新しい wire は要らない。ただし分類は webui でなく instance の fold に置くべき (ccmsg DR-0009「分類は daemon が導出」)。

### 2.3 サーバの理由文をそのまま出す

- harnessrouter (事実): HTTP エラーはサーバ JSON の `detail` を優先して出し、status 番号と生 JSON は出さない (`ui/src/lib/chat.ts`)。
- ccmsg-webui (事実): `src/refusal.ts` があり、断りを扱う層は既にある。中身が同じ方針か (理由文を素通しか、webui 側の言葉に置き換えるか) は未確認。
- 評価: 既に近い形なら取り込む物は無い。確認だけ要る (§5)。

### 2.4 取り込まない方がよいもの (ccmsg 向け)

- **reconcile poll (4 秒)**: harnessrouter は SSE を best-effort と割り切って poll で修復する。ccmsg は購読直後に必ず snapshot が来る契約で、意図しない切断は「次の snapshot が同じ行を置き換える」(ccmsg-webui DESIGN「切断は 3 つあり」)。poll を足すと同じ状態の 2 経路目になり ccmsg `docs/DESIGN-ja.md` の M2 に反する。harnessrouter が poll を要するのは、bus が進行中ターンしか replay せず再接続位置も持たないからで、構造上の穴を埋める手当てである。
- **進行中ターンの replay buffer**: ccmsg の `transcript.items:<sid>` は購読時に末尾 200 item の snapshot を運ぶので、既に同じ問いに答えている。

## 3. llm-gateway (events / stats) が参考にできる点

### 3.1 トークンの規約を 1 つに揃える (fresh / cached / output を足さない)

- harnessrouter (事実): ベンチの規則 4。`input_tokens` は fresh input に揃え、harness ごとの違いは runner で差し引いて吸収する。cached は別列、決して合算しない (`scripts/benchmark/README.md`)。
- llm-gateway (事実): stats の `tokens` は `input` / `output` / `input.cache_read` 等を別キーで持つ (`docs/MANUAL-ja.md` の `/llm-gateway/stats`)。一方、OpenAI Responses の `usage.input_tokens` は **cached を内数に含む総数**で、足し戻さず届いた値のまま持ち、単価側 (`crates/llm-gateway/src/preset/pricing.rs` の `OPENAI_REFINEMENTS`) が親から引いて 1 度だけ課金する (`crates/llm-gateway/src/preset/openai/metering.rs` のテスト `keeps_the_responses_input_total_as_it_arrives`)。
- 評価: USD は正しく出ているが、**stats の `tokens.input` という同じキーが、Anthropic 系の行では fresh、codex (Responses) の行では総数**を意味している可能性が高い。`--by origin` で `main` と `codex` を並べた時、input の桁が比べられなくなる。harnessrouter はまさにこの食い違いを「規約」として固定し test で縛っている。llm-gateway でも、保存の形は触らず集計の出口で「input = fresh」に揃えるか、欄の意味を MANUAL に明記するかの判断が要る。推しは前者 (読む側がエンドポイントごとに意味を覚えずに済む。DR-0012 の「時刻の単位を全 JSON で揃える」と同じ理屈)。stats の実際の値で裏取りしてから決めること (§5)。

### 3.2 捨てたものを数えて、見える所に出す

- harnessrouter (事実): 設計上の取りこぼし (bus の背圧、計量の送信失敗) を `_OpsDrops` の累積カウンタに数え、`/readyz` に載せる。コメントで「by-design-lossy な経路の可視化手段」と位置付けている (`gateway/app.py` 82-98 行)。
- llm-gateway (事実): events は broadcast 256 件で、追いつけない購読者の分は落とし、落とした数はログに残す (`docs/decisions/DR-0012-request-events.md`「見ている人がいなければ何もしない。遅れたら落とす」、実装は `crates/llm-gateway-server/src/lib.rs` と `crates/llm-gateway/src/webhook.rs` の `RecvError::Lagged`)。ログ以外の出口は見当たらない。
- 評価: ccmsg の cache のリングは events に依存しているので、落とした瞬間にリングが古い約束のまま回り続けうる。落とした数を `/llm-gateway/status` 等の既存 JSON に累積値で載せれば、「リングが怪しい時に gateway 側で落としていたか」を curl 1 発で切り分けられる。新しいエンドポイントを増やす必要は無い。推し (小さく、DR-0012 の「転送の邪魔をしない」を崩さない)。

### 3.3 集計が「部分的」であることを数字の横で言う

- harnessrouter (事実): 単価表の読み込みに失敗したら黙って 0 を出さず、そう言う (`gateway/app.py` の `_run_credits` 付近コメント)。ベンチの Notes は「served model unreported on 50 of 50 runs」のように、規則を検査できなかった run の数を行に書く (`docs/benchmark.md`)。
- llm-gateway (事実): `total_usd` は単価表にあるモデルの分だけを足し、1 行も出せなければ欄ごと省く (`docs/MANUAL-ja.md` の stats)。一部のモデルだけ単価が無い日は、`total_usd` が全体額に見える形で出る。
- 評価: 「単価が無くて足していない request 数 / モデル名」を日ごとに添えれば、部分和であることが数字の横で分かる。ccmsg-webui の費用画面 (DESIGN「費用は日ごとの記録を、読む単位に畳む」) も、その印を出せるようになる。origin の `unknown` 件数も同じ扱い (「内訳を持たない過去の分がこれだけ」) にできる。

### 3.4 要求した model と実際に応答した model

- harnessrouter (事実): ベンチの規則 2。応答が名乗る model と要求 id を `samemodel.py` で比べ、別 family / 別 tier なら finding。報告が無い run 数も数える。
- llm-gateway (事実): events / stats に `model` はあるが、それが要求側か応答側かは MANUAL からは読み取れなかった (§5)。
- 評価: gateway は応答本文を読んで usage を取っているので、応答側の model も同じ場所で取れるはず。食い違いを events に載せられれば、upstream の黙った差し替え (別 tier へのフォールバック等) を観測できる。価値は中程度。実装位置を確かめてから検討。

## 4. 参考にしない方がよい点と理由

- **フレームワークと依存の量**: Next.js + Tailwind + 2 つの図 library (echarts と chart.js) + cytoscape + framer-motion + axios。kawaz の webui 方針 (claude-rules-personal `memory/webui-modern-platform-first.md`: モダン CSS / 標準の振る舞いに任せる、対象は最新 Chrome と Safari のみ) とも、ccmsg-webui の Preact + signals + Vite の静的サイト、「図の library は入れない」(DESIGN「費用は日ごとの記録を…」) とも逆向き。
- **CSS の積み増し**: `hr.css` / `v2.css` / `revamp.css` と改修ごとに層を足しており、色の語彙を 1 か所で算出する ccmsg-webui DR-0001 と相容れない。
- **相対時刻を描画のたびに計算**: `TracesSidebar.jsx` / `TracesMain.jsx` の `relTime` / `fmtAgo` は描画時に `Date.now()` を読む。ccmsg-webui は丸めた時刻を粒度ごとに配る時計 1 本で、表示が変わる時だけ描き直す (DESIGN「Timeline の 3 層」の `src/now.ts`)。こちらの方が進んでいる。
- **SSE の再接続を固定間隔の自前ループで書く**: `subscribeHarnessEvents` は fetch + ReadableStream + 1.5 秒待ちで、再開位置も持たない。ccmsg は WS + snapshot 契約で既に解決済み。
- **poll による修復**: §2.4 の通り。M2 違反で、真因 (再開位置を持たない配送) の手当てにとどまる。
- **課金 (credits) 中心の情報設計**: harnessrouter の利用画面は SaaS 課金の都合 (credits、top up、pricing 表) が主語。llm-gateway / ccmsg は個人のクオータと cache 寿命が主語で、画面の問いが違う。

## 5. 未確認・要裏取りの点

- llm-gateway の stats で、codex (Responses) の行の `tokens.input` が実際に cached を含む総数で保存・表示されているか。単価側で差し引いていることはコードで確認したが、stats の出力値は実機で見ていない (§3.1 の前提)。
- llm-gateway の events / stats の `model` が要求側の値か応答側の値か (§3.4)。
- ccmsg-webui `src/refusal.ts` が理由文をどう扱っているか (§2.3)。
- harnessrouter の通知 (ヘッダの notifications) の中身と、失敗時にユーザへどう知らせるか。設計文書が見当たらず、コードも追っていない。
- harnessrouter の Traces 画面が進行中セッションを live で更新するか (bus を購読しているか)。`ui/src/studio/traces/api.js` と `store.js` は読んでいない。
- harnessrouter の UI は起動しておらず、見た目・操作感は未確認。
