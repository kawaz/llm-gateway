# harnessrouter の runner (ハーネス駆動) 調査

対象: HarnessRouter/harnessrouter (main、2026-09-23 時点の clone)。観点は `runner/` と各 harness adapter。比較対象は kawaz/ccmsg、kawaz/llm-gateway (daemon / discovery)、kawaz/claude-rules-personal の worker 運用 (`reference/delegation/model-effort-matrix.md`、`agents/`)。

## 1. 対象の要約 (事実)

### 全体構造

- runner は `runner/server.py` (7606 行、FastAPI) 1 本に 14 種の backend (claude / codex / hermes / dsh / pi / omp / qwen / kimi / aider / gemini / cline / opencode / goose / openhands / systemone) を登録表 `BACKENDS` で持つ。補助 driver が `runner/aider_driver.py` / `runner/dsh_driver.py` / `runner/openhands_driver.py` / `runner/systemone_driver.py`、MCP の中継が `runner/mcp_bridge.py` / `runner/aider_mcp_bridge.py`。
- 前段に `gateway/` (`gateway/app.py` 他) があり、gateway が sandbox を割り当てて runner の HTTP API を叩く。runner の冒頭 docstring は「1 turn = 1 backend CLI の one-shot 実行を /workspace 上で行い、イベントを 1 つの正準スキーマ (Claude Code の `stream-json`) に正規化する」と述べる。
- runner の API (`server.py` 後半): `POST /turn` (非同期起動、即座に turn_id を返す)、`GET /turn/{id}?since=N` (イベントの差分 poll)、`POST /turn/{id}/cancel`、`POST /hydrate` / `GET /checkpoint` (workspace の tarball 入出力)、`GET /produced` / `POST /produced/ack` (生成ファイル)、`GET|PUT /file`、`DELETE /workspace`、`GET /capabilities`。

### 起動引数と stdin/stdout プロトコル

- Claude Code (`_build_claude`): `claude -p <prompt> --output-format stream-json --verbose --dangerously-skip-permissions --max-turns N [--include-partial-messages] [--mcp-config ...] [--plugin-dir ...] [--resume <id>] --model <m>`。`CLAUDE_CONFIG_DIR` を `$HOME/.claude` (= `<workspace>/.harness/home/.claude`) に固定。
- 無効化ツールは `--disallowedTools` でなく `settings.json` の `permissions.deny` に書く。コメントに「`--dangerously-skip-permissions` が permission 系を丸ごと無効にするので flag は受理されるが効かない、両方向で実機確認した」とある。
- Codex (`_build_codex`): 既定は `codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check --json -c model=<m> --cd <cwd> <prompt>`、再開は `codex exec resume --last ...`。`--ephemeral` を外して rollout を `$CODEX_HOME/sessions` に残す。
- Codex app-server 経路 (`_run_codex_appserver_bg`、flag で選択): `codex app-server` を 1 turn 1 プロセスで起こし、stdin/stdout の JSON-RPC で `initialize` → `initialized` → `thread/start` or `thread/resume` → `turn/start` を順に送る。`clientInfo.name` は `harness-runner`。`item/agentMessage/delta` を文字単位のストリームとして拾い、`turn/completed` で終える。コメントでは「codex で assistant text を逐次ストリームできる唯一のモード」。`thread/resume` にも今回の model と sandbox/approvalPolicy を渡す (`_codex_thread_request`、「bare に resume すると最初のモデルのツール集合のままになる」と実測記録あり)。
- hermes はイベントを stdout に出さないため、CLI を起こして `state.db` (sqlite) を 0.8 秒間隔で tail し、正準イベントを合成する (`_run_hermes_bg`)。
- 全 backend とも stderr を stdout に merge し、`{` で始まらない行を直近 80 行の `errbuf` に保持して失敗理由に使う (`_run_turn_bg`)。

### 認証の渡し方

- 認証は turn のリクエストボディ `Auth` から backend ごとの env / 設定ファイルに翻訳する (`_build_claude` の anthropic / bedrock / vertex / tokenrouter 分岐、codex は `config.toml` の `[model_providers.*]`)。
- runner 自身の env から秘密らしい名前 (`_SECRET_ENV`: `_API_KEY$` / `_TOKEN$` / `PASSWORD` 等) を子に継承させない。呼び出し側が渡す env は予約接頭辞 (`HR_` / `OPENAI_` / `ANTHROPIC_` / `LD_` 等) を拒否 (`_caller_env`)。turn の秘密値はイベント記録から scrub する (`_turn_secrets` / `_scrub_secrets`、8 文字未満は対象外)。
- 一部 backend は loopback の relay (`_HermesRelayHandler`) を経由させ、provider への実リクエストを中継しながら body の方言差を補正し、応答の `model` と usage を読み取って turn の result に後付けする (`_relay_served_model` / `_relay_usage`)。

### セッションの永続化と再開

- CLI の会話状態 (Claude の `projects/*.jsonl`、codex の rollout) は `$HOME` を workspace 内に向けることで workspace ごと checkpoint される (`/checkpoint` は git commit + tar、`/hydrate` は spool 完了後に wipe して展開)。別 sandbox で再開できる。
- 再開時は「会話ファイルが実在するときだけ `--resume`」。無ければ同じ workspace で新規に始め、その事実を turn の先頭イベント (`system/resume_lost`、codex は固定文 `_CODEX_NO_ROLLOUT_NOTE`) で明示する (`_resume_lost`、`_SESSION_PRESENT`)。「黙って空の履歴で答えた」事故 (2026-09-08) が理由として記録されている。
- codex の rollout は再生前に reasoning blob を落とすなど sanitize する (`_sanitize_codex_rollout`)。

### 進捗のストリーミング

- runner 内は turn ごとの record にイベントを append し、gateway が `GET /turn/{id}?since=N` で差分を poll する。完了 record は `MAX_TURN_SECONDS + 30 分` で evict (`_evict_turns`)。
- `POST /turn` は `idempotency_key` で重複起動を防ぐ (lock の外と内で二重確認)。

### cancel / timeout / 異常終了

- 子は `start_new_session=True` で独立プロセスグループ。cancel / timeout は `killpg(SIGKILL)` の後、env に埋めた `HR_TURN_ID=<id>` を `/proc/*/environ` から探して残党も殺す (`_kill_proc_tree` / `_sweep_turn_processes`)。setsid で逃げる tmux サーバの実例が理由として書かれている。
- spawn と cancel の競合は「spawn 直後に cancelled フラグを見て即 kill」で閉じる。
- timeout は `threading.Timer(min(timeout_seconds, MAX_TURN_SECONDS))` (既定上限 6h)。
- 終了状態は `cancelled` > `timeout` > result イベントから導出、の優先順。失敗理由は「provider の拒否行 > result の error > CLI の末尾行 > exit code」で 1 行に決める (`_failure_reason`)。CLI が人間向けに出す「To resume this session: ...」等は除く。
- 終了した turn のパイプを閉じないと fd が枯渇した実例 (506 turn で 1020/1024) があり、`_release_proc` で閉じる。

### files / artifacts

- 入力ファイルは turn 前に workspace へ書く (`_write_input_files`、`_safe_join` で脱出防止)。
- 生成物は git の差分で判定し、`refs/hr/collected` を既読位置として `/produced` と `/produced/ack` で受け渡す。`.harness/` や `node_modules` 等は除外 (`_PRODUCED_EXCLUDE_*`、`_is_produced_noise`)。

### sandbox と作業ディレクトリの分離

- 2 つの配置不変条件を env で宣言し、未宣言時は閉じる側に倒す: `HR_SANDBOX_PER_SESSION` (1 session 1 コンテナ) と `HR_SESSION_UIDS` (1 runner が全 session を持つ自己ホスト時、session ごとに uid を割り当て、session dir を 0700 で所有させる)。両者は排他で、後者は root でなければ起動を拒否する。
- コメントの論旨は「指示で縛らず、書けない場所は EACCES で書いた瞬間に失敗させる」。

### 並列実行とリソース

- turn はスレッド 1 本ずつ。並列数の上限を runner 内に持つ実装は見当たらない (gateway 側は未確認)。資源の上限は turn の時間上限、fd の解放、spool の掃除、workspace の TTL 回収 (`HR_WORKSPACE_TTL_HOURS`、既定 72h) で押さえている。

### 検証方法 (`docs/harness-verification.md` / `docs/support-matrix.md`)

- harness × model ごとに 5 シナリオ (初回 / 追撃 / 途中モデル切替 / artifact / sandbox を捨てた後の recall) を `scripts/support-matrix/run.mjs` で回し、`docs/support-matrix.md` (3423 行の表) と `docs/support-matrix-results.json` に出す。
- 「完了した」だけでは pass にしない 4 規則をコードで強制: 指定した接続で走ったか (`EXPECT_CONNECTION`)、要求したモデルが実際に served されたか (`scripts/support-matrix/samemodel.py`、vendor prefix と日付 suffix だけ同一扱い)、表示した file カードが保存物と名前・件数とも一致するか、catalog が実行不能なモデルを出していないか。
- custom harness 検証 (`scripts/support-matrix/custom-harness.mjs`) は、実行ごとに生成するトークンを skill 同梱スクリプトの中にだけ置き、答えにそのトークンが出れば「bundle が届いた」、スクリプトが書くファイルが生成物にあれば「実行された」と判定する。MCP は答えの文面でなく tool call の記録で判定し、外部 MCP が落ちていれば「その半分を skip」と明記する。
- 運用規則: 1 列 1 provider (隔離 = 他 integration の削除)、実行中は deploy 禁止、`incomplete` は除外前に再試行して provider のエラー文を記録。

## 2. ccmsg が参考にできる点

前提 (事実): ccmsg は子プロセスとして harness を駆動しない。README と `docs/DESIGN.md` §4.1 によれば、ccmsg は config home 内のファイル (transcript / rollout / lock) と接続を入力に、既存 session を観測し、配送は Claude Code の messaging socket か `codex queue --thread <sid>` (route (a)) と topic/inbox (route (b)) で行う。harnessrouter と重なるのは「複数 harness の差を 1 つの正準表現に吸収する」「codex / claude の session 同一性と再開の扱い」「外部子プロセスの deadline」の部分。

### 2.1 再開不能を黙らずイベントで言う (`resume_lost`)

- harnessrouter (事実): 再開要求の session が実在しなければ新規で走らせ、turn 先頭に `system/resume_lost` イベントを置く。codex は固定文を assistant text として挿入。
- ccmsg の現状 (事実): DESIGN §4.3 は、lost な row に「resume が何として resume すべきか (`model` と `effort`)」を載せる。二重 run は `frozen` にして隠さない (§4.3 "Two runs of one session are shown, not resolved")。ccmsg 自身は resume を実行しない。
- 評価: ccmsg は「壊れた読みを真実として述べない」方針が既に同型で、概念としての新規性は小さい。取り込む余地があるとすれば、Claude Code / codex が resume に失敗して新 sid で走り直したケース (sid が変わる) を、ccmsg が「同じ作業の続き」と誤って見せないかの確認。rollout の `<thread-id>_<rollout-id>` を前半で同一視する規則 (§4.1) と組み合わせて検証する価値はある。
- 取り込まない方がよい理由: ccmsg は resume の実行主体でないので、`resume_lost` 相当のイベントを ccmsg が生成すると「上流の判断を再導出しない」(README の "What it does not do") に反する。上流が言ったことを伝えるに留めるべき。

### 2.2 子プロセスの後始末 (プロセスグループ kill + env マーカーによる掃討)

- harnessrouter (事実): 子を新しいプロセスグループで起こし、killpg の後に `HR_TURN_ID` を env に持つ残党を `/proc` から探して殺す。
- ccmsg の現状 (事実): DESIGN §1.3 の表に launcher の強制 kill (timeout 後 SIGTERM、500ms で SIGKILL、`FORCE_KILL_MS`) があり、`codex queue` の子は stdin なし・deadline 付き (§4.1)。プロセスグループ単位かどうかは docs からは読み取れなかった (未確認)。
- 評価: ccmsg が起こす子 (`codex queue` や launcher 経由のコマンド) が孫を残すと、deadline 後も pipe が開いたままで読み手が止まる、という harnessrouter が踏んだ罠 (`_kill_proc_tree` の docstring) はそのまま当てはまりうる。プロセスグループ kill の有無をコードで確認し、無ければ入れる価値は高い。env マーカー掃討は macOS に `/proc` が無く、そのままは移植できない (`ps -E` 相当が必要で重い)。
- 取り込まない方がよい理由: env マーカー掃討は setsid で逃げる子 (tmux 等) を想定したもので、ccmsg の子 (`codex queue` の短命実行) にそこまでの必要があるかは不明。まずグループ kill だけで足りるかを実測してから。

### 2.3 finished な子の fd を閉じる

- harnessrouter (事実): 終了 turn の pipe を閉じずに record に保持し続けて fd 枯渇した実例と対策 (`_release_proc`)。
- ccmsg の現状: 長期常駐 (DR-0013) で `codex queue` を配送ごとに起こすので、同型の漏れがあれば常駐期間に比例して効く。該当コードは今回読んでいない (未確認)。
- 評価: 実装確認のコストが低く、事故時の影響 (全配送停止) が大きい。点検項目として有用。

### 2.4 失敗理由を 1 行に決める優先順

- harnessrouter (事実): provider の拒否行 > 構造化 error > CLI の末尾行 > exit code。CLI が人に向けて出す再開ヒント等は除外。
- ccmsg の現状 (事実): DESIGN §6.6 が非配送理由とその決定箇所を定める。route (a) が落ちる条件 (flag 無効 / socket なし / key 読めない / 世代不一致 / ack timeout) で (b) に落ちる (§9.4)。
- 評価: ccmsg の非配送理由は既に列挙型で、harnessrouter より構造化されている。取り込むとすれば `codex queue` が非ゼロ終了したときに stderr の末尾を理由に添える部分だけ。

### 2.5 codex の app-server JSON-RPC を使う経路

- harnessrouter (事実): 逐次ストリームが要る時だけ `codex app-server` を 1 turn 1 プロセスで起こし、`thread/resume` に model 等を必ず渡す。
- ccmsg の現状 (事実): DESIGN §4.1 は「Codex の待ち状態 (`WaitingOnApproval` / `WaitingOnUserInput`) は app-server の `thread/status/changed` でしか分からず、購読が要る JSON-RPC は §4.2 の入力の種類に無いので足さない」と明記。
- 評価: harnessrouter は自分で起こした app-server に対して JSON-RPC を話しており、他人 (人が起こした interactive codex) の thread の状態を購読する用途には使っていない。したがって ccmsg の「足さない」判断を覆す材料にはならない。参考になるのは、app-server の通知名 (`item/agentMessage/delta`、`turn/completed`、`thread/tokenUsage/updated`) と、トークン使用量の通知名を形で照合してリネームに耐える実装 (`'token' in method and 'usage' in method`) くらい。

### 2.6 「正準イベントに正規化し、client は生フォーマットを読まない」

- harnessrouter (事実): 全 backend を Claude Code `stream-json` 形に正規化 (`_codex_to_claude`、`_pi_to_claude` 等)。
- ccmsg の現状 (事実): DR-0002 と DESIGN §5.5 で「client は生の jsonl を読まない、契約が語彙を持ち daemon が分類する」。codex rollout も同じ分類で吸収。
- 評価: ccmsg は正準形を独自語彙 (契約の item 型) で持っている点で harnessrouter より筋が良い。harnessrouter は特定 harness の形 (Claude の stream-json) を正準にしているので、Claude 側の形式変更が全 backend に波及する。取り込む点はない。

### 2.7 検証マトリクスの判定規則

- harnessrouter (事実): 「完了した」を pass にせず、served model / 接続 / 表示と保存の一致をコードで強制。外部依存が落ちていれば skip と明記。
- ccmsg の現状 (事実): DESIGN §9 (契約 fixture 共有、認可境界の直接テスト、「育てない」を壊す変更の検知、配送、mesh)。harness 横断 (claude × codex) のマトリクスは docs に見当たらない。`docs/findings/2026-09-09-codex-session-delivery-path.md` のような版指定の実測は個別にある。
- 評価: ccmsg は claude と codex の 2 harness × route (a)/(b) × 状態 (idle / busy / 待ち / 二重 run) で、「配送した」でなく「相手の transcript に届いた」を判定する小さなマトリクスを持つと、codex 版上げのたびの退行検知になる。harnessrouter の「答えではなく記録 (tool call / transcript) で判定」「生成トークンで由来を証明」は、ccmsg の到達確認にもそのまま使える発想。
- 取り込まない方がよい理由: 実 harness を起こす E2E はコストと不安定さ (認証・課金・版差) を持ち込む。CI でなく手動 runbook + findings 記録の粒度に留めるのが妥当と考える。

## 3. llm-gateway / kawaz の worker 運用が参考にできる点

### 3.1 runner 内 loopback relay で served model と usage を読む

- harnessrouter (事実): CLI が served model や usage を報告しない backend でも、relay が provider 応答の `model` と usage を読み、turn の result に後付けする。検証規則 2 (要求したモデルが served されたか) はこれで全 backend に効くようになった、と `docs/harness-verification.md` に書かれている。
- llm-gateway の現状: llm-gateway 自体が proxy として同じ位置にいる (stats に origin × credential × model を記録)。
- 評価: 発想は一致しており、llm-gateway が既にやっていることの裏付け。worker 運用側では「Agent tool で起こした codex worker が本当に agent 定義の model (例 `gpt-6-sol`) で動いたか」を llm-gateway の stats で照合できる、という使い方が harnessrouter の規則 2 に相当する。agent 定義の model 固定方針 (`model-effort-matrix.md`「agent 定義の固定方針」) の実効確認手段として有用。

### 3.2 client の名乗りと origin 判定

- harnessrouter (事実): app-server に `clientInfo.name = "harness-runner"` を名乗らせる。
- llm-gateway の現状 (事実): `crates/llm-gateway/src/discovery.rs` は catalog 取得時に codex の版 (`CODEX_CLIENT_VERSION = "0.155.0"`) を名乗る。`docs/issue/2026-09-23-codex-cli-traffic-recorded-as-unknown-origin.md` は、Agent tool 経由の `codex exec` の通信が origin `unknown` に積まれる件を追っている。
- 評価: harnessrouter は codex を `exec` と `app-server` の 2 経路で起こしており、経路によって codex が使う入口 (HTTP の Responses か別経路か) が変わりうる。上記 issue の裏取りで「`codex exec` と `codex app-server` で入口が違うか」を観測軸に加える価値がある (未検証)。

### 3.3 `permissions.deny` と `--disallowedTools` の実効差

- harnessrouter (事実): `--dangerously-skip-permissions` 下では `--disallowedTools` が効かず、`settings.json` の `permissions.deny` なら効いた、と両方向の実機確認がコメントにある。
- kawaz の運用: worker 定義 (`agents/*.md`) の frontmatter はツール制限を持たず、reviewer は「読み取り専用」を本文の指示で担保している (reviewer-sol-high の description)。
- 評価: 「読み取り専用」を指示でなく実効的に縛りたくなった時、どの層で縛れば実際に効くかの一次情報として有用 (Claude Code の Agent tool 経由 worker に同じ差があるかは未検証)。harnessrouter の「指示は従う model しか縛らない」(`HR_SESSION_UIDS` のコメント) という論旨は、reviewer に書き込みさせない保証を指示に頼っている現状への問いになる。

### 3.4 idempotency key と spawn/cancel 競合

- harnessrouter (事実): `POST /turn` の重複起動を key で弾き、cancel が spawn と競合したら spawn 直後に kill。
- llm-gateway の現状 (事実): supervisor (`crates/llm-gateway/src/daemon/supervisor.rs`) は SIGTERM → 10 秒で SIGKILL、指数 backoff (1s〜60s)、healthz で backoff を戻す。子は `daemon run <unit>` の単一プロセス。
- 評価: supervisor は「unit 名で 1 つ」が自然な冪等性になっており、key は不要。stop と起こし直しの競合 (epoch で管理している様子) は既に考慮済みに見える。参考度は低い。

### 3.5 検証マトリクスを worker 選定の実測に使う

- harnessrouter (事実): 同一タスクを 8 harness × model 構成で走らせ、cost と latency の最良・最悪を比較 (`docs/benchmark.md`、README)。
- kawaz の運用 (事実): `model-effort-matrix.md` は「公開ベンチ数値でなく役割構図で選ぶ」「sol / astra は未実測・評価は実務で更新」と書く。
- 評価: harnessrouter の方法 (同一課題、served model 確認、記録で判定) を小さく真似ると、「astra は sol の 2 倍コスト、未実測」のような空欄を埋められる。llm-gateway の stats がコスト側の実測を既に持つので、課題セットと判定規則を足せば成立する。ただし kawaz の選定軸は「難易度」で、単一課題のコスト比較は軸の一部しか埋めない。

## 4. 参考にしない方がよい点と理由

- **Claude Code の stream-json を正準形にすること**: 特定 harness の内部形式を正準にすると、その harness の形式変更が全体に波及する。ccmsg の「契約が語彙を持つ」(DR-0002) の方が責務境界として正しい。
- **7606 行の単一ファイルに 14 backend を同居させる構成**: backend ごとの方言補正 (Gemini のスキーマ変換、thought signature、max_tokens の改名等) が一つのモジュールに積もっている。kawaz の設計指針 (責務分離) から見て真似る対象ではない。
- **hermes の sqlite を 0.8 秒間隔で poll する駆動**: 上流が stream を出さないための次善策で、kawaz の sloppy-ai-patterns (polling 禁止、例外は polling しか無い外部) の例外に当たる。ccmsg は fs watch を使っており (`docs/findings/2026-09-15-fs-watch-reliability.md`)、逆行する理由がない。
- **gateway → runner の進捗を HTTP の差分 poll で取る形**: 長時間 turn に対し接続を持たない設計としては妥当だが、ccmsg は WS / UDS の push を持っているので置き換える理由がない。
- **`--dangerously-skip-permissions` / `--dangerously-bypass-approvals-and-sandbox` を常用する前提**: harnessrouter は sandbox (コンテナ or uid 壁) を境界にしているから成立する。kawaz のローカル worker は host 上で走り、同じ前提を持たない。

## 5. 未確認・要裏取りの点

- gateway 側 (`gateway/app.py`) の並列上限・キュー・sandbox プールの扱いは読んでいない。runner 単体に並列上限は見当たらなかったが、全体としての資源制御は未確認。
- ccmsg の子プロセス起動 (launcher / `codex queue`) がプロセスグループで kill しているか、終了後に pipe を閉じているかはコードを読んでいない (docs の記述のみ確認)。
- Claude Code の Agent tool 経由 worker で、`--disallowedTools` と `permissions.deny` に harnessrouter と同じ実効差があるかは未検証。
- `codex exec` と `codex app-server` で codex が upstream へ出る入口が違うかは未検証 (llm-gateway の origin `unknown` issue の観測軸候補)。
- `docs/support-matrix.md` の各行が何日・どの版で測られたかは本文から読み取れず、`docs/support-matrix-notes.md` / `docs/support-matrix-results.json` は精読していない。
- harnessrouter の codex app-server 経路で使う JSON-RPC の method 名は runner のコードから読んだもので、codex の公開仕様との突き合わせはしていない。
