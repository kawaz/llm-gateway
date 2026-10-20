# tap で本文を取って jsonl 記録・stats と突き合わせる、tap の限界 (max_body / メモリ / 切断)

- Date: 2026-10-21
- Status: Concluded (不明点は末尾に列挙)

## 動機

[直前の調査](2026-10-21-claude-code-api-request-records.md) で、Claude Code 2.1.300+ の jsonl に残る api-request 記録から送信 body を復元できると分かった。ただし gateway 側で本文を取る手段を使わずに突き合わせたので、次の 2 点が残った。

- gateway の tap (DR-0017) で本文を取ると、記録から復元した body とどこが違うか。tap が控えるのは gateway が受けた形か、upstream へ送った形か
- tap で「数分だけ全量を取って後で突き合わせる」が実用になるか。[DR-0038](../decisions/DR-0038-timed-request-dump.md) (一時ダンプ) を作るか tap で済ませるか (`docs/QUESTIONS.md` DP-Q1) の判断材料として、tap の限界を数字で出す

あわせて、前回の不明点のうち Claude Code のコードを読めば答えられるもの (`role:"system"` の出どころ、subagent で重複判定が外れる理由、`user_identity` の出どころ) を片付ける。

## 調査範囲

- gateway: unstable unit (127.0.0.1:11301、0.68.0、`target/release` の binary。tap 関係のコード `tap.rs` / `exchange.rs` / `llm-gateway-server/src/lib.rs` / `cache.rs` の最終変更は binary のビルドより前)。設定は `~/.config/llm-gateway/config-11301-unstable-new.toml` の `ns.personal`
- クライアント: Claude Code 2.1.304 を `claude -p --model claude-haiku-4-5-20251001` で実行。`CLAUDE_CONFIG_DIR` は `mktemp -d` の下に作り、その `settings.json` の `env` に `ANTHROPIC_BASE_URL=http://127.0.0.1:11301/ns-personal` と、token helper で鋳造した ns の JWT を書いた
- Claude Code のコード: `~/.local/share/claude/versions/2.1.304` を bytes の検索で読んだ
- 扱わないもの: prompt・thinking・tool の出力の本文 (構造と大きさだけを見る)。stable unit

tap は unit 全体の交換を流すので、購読中は kawaz の稼働中のセッションの本文も入る。集計はモデル・origin・大きさ・`cache_control` の位置などの構造だけにし、取った jsonl は一時ディレクトリごと削除した。

## 調査メモ

### 2026-10-21: A1〜A2. tap で request と response を揃える

- `curl -sSN 'http://127.0.0.1:11301/llm-gateway/tap?include=request_body,response_body&max_body=50000000' > tap.jsonl` を張った状態で、Read を 2 回使わせる 1 本の `claude -p` を流した。exit 0、`num_turns` 3、session `aa8566ca`
- tap に 3 行 (3 リクエスト) が出て、3 行とも `request_body` と `response_body` が揃った。request は 84592 / 85820 / 86863 bytes、response (SSE) は 6081 / 5316 / 3630 bytes で、`request_body_size` / `response_body_size` と一致した (切り詰め無し)
- `origin` は 3 行とも `oneshot`、`cache_strategy` は `passthrough`、`cache_ttl_secs` は 300。`claude -p` は請求ヘッダが `cc_entrypoint=sdk-cli` なので `session/origin.rs` が oneshot に分け、`ns.personal.cache` の `sub` 側 (既定の passthrough) が効く。つまり `claude -p` では gateway が `ttl:"1h"` を足さない
- `ts` は Unix 秒で出ている (例 `1792512404`)。MANUAL の運用系の約束 (時刻の欄は Unix ミリ秒) と MANUAL の tap の例 (`1785326400000`) に反する。コードは `now_unix()` (秒) を入れている (`llm-gateway-server/src/lib.rs` の `tap::Event` の組み立て)
- `ts` は応答ヘッダを受けた時点、行が出るのは応答の終端なので、ファイル上の並びは終わった順になる

### 2026-10-21: A3. tap の request_body は gateway が受けた形

tap の 3 行と、同じ実行の jsonl (`$CLAUDE_CONFIG_DIR/projects/.../aa8566ca-….jsonl`) の api-request 3 件を、直前の調査と同じ規則で突き合わせた。

| 照合 | 結果 |
|---|---|
| shape: tap body から billing ブロックを除いた `{system, tools}` (cache_control 除去) の sha256 と `shapeHash` | 3/3 一致 |
| messages: tap body の各 message の sha256 と、`base` の連鎖を `keep` で切って `tail[]` を足した列 | 3/3 一致 |
| `requestDigest`: tap body から `metadata`・`stream`・billing ブロックを除き、`params.betas` を足した正規化 JSON の sha256 | 3/3 一致 |
| `cacheControl` の path と値 (billing を除いた配列での位置) | 3/3 一致 |

tap body と記録から復元した body の差は、`metadata` (176 bytes)、`stream: true`、billing ブロック (system[0]、101 bytes)、`betas` (記録は params に持ち、実際はヘッダ `anthropic-beta`) だけだった。HTTP ヘッダは tap にそもそも載らない。前回報告の 5 項目のうち、gateway が足す ttl は tap にも現れない (下記)。

tap が受けた形であることの根拠は 3 つ。

- コード: `forward` は受けた bytes を `tap::capture(&bytes, plan.request_body)` で控え、そのあとで `apply_thinking_display`、`forward_as` の中の model 解決・`cache::apply` を当てる (`llm-gateway-server/src/lib.rs` の「tap に載せる値は gateway が書き換える前の client request から取る」)
- thinking: Claude Code は `thinking: {type: enabled, budget_tokens: 31999, display: "omitted"}` を送っており、tap body も `omitted` のままで、`requestDigest` も `omitted` で一致した。ところが `ns.personal` は `thinking_display = "summarized"` で display を上書きして送る。実際に応答には thinking_delta の本文が 129 / 52 / 108 文字返っていた (omitted なら空になる)。upstream が受けたのは summarized で、tap に載ったのは書き換える前の形
- ttl: 同じ時間帯に tap に入った稼働中の main の 1 行 (fable、`cache_strategy: keepalive`、`cache_ttl_secs: 3600`) は、request_body のブレークポイント 4 点がすべて `{"type":"ephemeral"}` で ttl が無かった。その応答の usage は `ephemeral_1h_input_tokens: 989` / `ephemeral_5m_input_tokens: 0` で、upstream には `ttl:"1h"` が届いている

したがって tap と Claude Code の記録はどちらも「Claude Code が gateway に送ったもの」を表し、gateway の加工 (ttl、thinking の display、model の解決、thinking_as_text の変換) はどちらにも載らない。加工の有無は `cache_strategy` / `cache_ttl_secs` の欄と response の usage から間接に読むことになる。

### 2026-10-21: A4. usage と cache_ttl_secs と stats

tap の response_body (SSE) の `message_start` / `message_delta` から usage を取った。

| リクエスト | input | cache_creation (5m / 1h) | cache_read | output (うち thinking) |
|---|---|---|---|---|
| 1 | 10 | 21526 (21526 / 0) | 0 | 153 (47) |
| 2 | 8 | 207 (207 / 0) | 21526 | 128 (22) |
| 3 | 8 | 180 (180 / 0) | 21733 | 46 (38) |

- 連鎖は cache_read(n) = cache_read(n−1) + cache_creation(n−1) で、前回の観察と同じ形 (21526 → 21526+207 = 21733)
- tap の `cache_ttl_secs` 300 と、usage の 5m 側だけに計上されている事実が合う
- stats (`~/.local/state/llm-gateway/stats/2026-10-21.127-0-0-1-11301.json`) の `claude-kawazzz / claude-haiku-4-5-20251001 / oneshot` は requests 4、input 36、cache_creation 81407 (ephemeral_5m 81407 / 1h 0)、cache_read 43259、output 1322。前回の調査の 1 本 (10 / 59494 / 0 / 995) と今回の 3 本の和 (10+10+8+8、59494+21526+207+180、0+0+21526+21733、995+153+128+46) にすべて一致した
- 1h 側の突き合わせは、上の main の 1 行で「tap の `cache_ttl_secs` 3600 ↔ usage の `ephemeral_1h`」までを確かめた。stats の 1h の内訳との照合は、稼働中のセッションの集計と分けられないのでしていない

### 2026-10-21: B1. max_body の上限

- コード: `max_body` は `usize` として parse するだけで上限を持たない (`tap_options`)。request の本文はその前に `axum::body::to_bytes(body, MAX_BODY)` (64 MiB) で読み切るので、request 側で実際に取れる上限は 64 MiB。response 側はコード上の上限が無い
- 実測: 5.5 MB のテキストを stdin から渡す `claude -p` で、request 5652503 bytes が tap に切り詰め無しで載った (`len(request_body) == request_body_size`)。upstream は prompt too long の 400 を返し、それでも tap の 1 行は出る (`forward_as` が Ok で upstream の status をそのまま載せる経路)。9.5 MB 版 (request 9701827 bytes) も同じく全量が載った
- `claude -p` は stdin が 10 MB を超えると送らずに終わる (`piped stdin input exceeds 10MB`) ので、これより大きい 1 本は試していない
- 既定の 64 KiB では実用にならない。今回 tap に入った稼働中のリクエスト 64 本はすべて 64 KiB を超えていた (中央値 554 KB、最大 2.59 MB)。response (2 本目の購読に入った 40 本) は中央値 18 KB、最大 61 KB で、既定でもほぼ収まる
- gateway の手前で失敗した 1 本 (ns 認証の拒否、namespace 不明、`forward_as` の Err) は tap に出ない

### 2026-10-21: B2. response の控え方とメモリ

コード (`exchange.rs` の `BodyObservation`):

- response は通り過ぎるチャンクを `response_body: Vec<u8>` に上限まで足していき、終端 (流し切った / 途切れた / クライアントが去って Drop) で `tap::capture` が `String` に写して 1 件にまとめて publish する。チャンクごとには出さない
- 購読者が居るときの 1 本あたりのコピー: request は控えの `String` 1 つ、response は `Vec` とその写しの 2 つ。broadcast の slot は全購読者が読み終えるまで 1 件を持ち、各購読者は `recv` で 1 件を丸ごと clone してから自分の `max_body` で切る (tokio 1.53 の `RecvGuard` の drop で最後の読み手が slot を空にする)。さらに 1 行を JSON にするとき本文がエスケープされた写しができる。切り詰めは clone の後なので、メタデータだけの購読者が同居していても、本文を取る購読者の最大 `max_body` 分は全員に clone される

RSS の実測 (unstable unit、`ps -o rss` を 0.1 秒ごと。同じ unit に稼働中のセッションのトラフィックが流れており、各条件 1 回ずつ):

| 条件 | 開始時 | ピーク | 増分 |
|---|---|---|---|
| 5.65 MB の 1 本、本文の購読あり | 79.0 MB | 106.6 MB | +27.6 MB |
| 9.7 MB x 並列 4 本、購読なし | 79.0 MB | 272.7 MB | +193.7 MB |
| 9.7 MB x 並列 4 本、本文の購読あり (curl をファイルへ) | 104.3 MB | 340.7 MB | +236.4 MB |

- 並列 4 本 (本文の合計 38.8 MB) で、購読ありは購読なしより増分で +42.7 MB、ピークで +68 MB 多い。本文の 1.1〜1.75 倍が tap の上乗せという見当になる。上のコピーの数え方と矛盾しない
- 購読の有無にかかわらず、gateway 自身が 1 本の request に本文の約 5 倍のメモリを使っている (購読なしで 38.8 MB に対し +194 MB)。tap の上乗せはその 2〜3 割
- どの条件でも、終わってから 2 分ほどで RSS は開始時の水準に戻った (例: 274 MB → 105 秒後に 86 MB)
- 「並列 4 本の `claude -p`」の通常の大きさ (85 KB 前後) では差が測定の揺れに埋もれるので、上の表は大きい本文で取った

### 2026-10-21: B3. 購読が追いつけず切断される条件

- 購読側が curl でファイルへ書いている限り、並列 4 本でも切断は起きなかった。2 本目の購読 (約 11 分) には 115 行が入り、自分の 4+4 本の大きい request と 71 本の小さい request がすべて揃っていた
- `BACKLOG` 64 は件数の上限で、バイト数ではない。tokio の broadcast は送り手を待たせず、読まれていない slot を古いほうから上書きする。遅れは購読者が次に `recv` した時に `Lagged` として分かり、そこで切断する
- 実測: 本文を取る購読者の curl を `SIGSTOP` で止めた。止めている間は切断されず (`recv` が呼ばれないので遅れを知らない)、購読者として数えられたままなので、全リクエストの本文の控えも続く。9.7 MB x 4 本を流した後、RSS は 351.9 MB のピークから 141 MB で約 1.5 分横ばいになった (開始時 101.7 MB、差 +39 MB ≒ 4 本分の本文が ring に残っている)。稼働中のトラフィックが加わって 152 MB まで増えた。小さいリクエストを 70 本流して ring を押し流し、`SIGCONT` で再開すると、curl は 1 行 (9.7 MB) を受け取った直後に切られ、unit の log に `tap subscriber lagged; disconnecting missed=23` が出た。切断後の RSS は 119.7 MB
- つまり危ないのは「止まったまま繋ぎっぱなしの購読者」で、最大 64 件分の本文 (最悪は 64 x 64 MiB) を unit に持たせ続ける。切断はその購読者が読み始めた時まで起きない

### 2026-10-21: B4. 購読者がいない時のコスト

- `capture_plan` は購読数 (`AtomicUsize`) が 0 なら lock もせずに `None` を返し、request の控え・response の `Vec` への追加・`Event` の組み立ては起きない。`publish` も購読数 0 なら何もしない。追加のコストは 1 本あたり atomic の load 2 回だけ
- メタデータだけの購読者しかいない時は `max_body` が 0 扱いになり、`capture` は本文を写さない (`max > 0` の条件)
- 計画はリクエストを受けた時点で決まるので、そのリクエストの途中から購読を始めても、その 1 本は (メタデータも含め) 流れない

### 2026-10-21: C1. `role:"system"` のメッセージの出どころ

- Claude Code 内部のメッセージ種別 `api_system` (`{type:"api_system", message:{role:"system", content}}` を作る関数がある) が、送信時に wire の `{role:"system", ...}` になる。作られる場面はコード上 4 つ
  - ターンごとの effort と時刻 (`outputConfig` → wire の `output_config`)
  - tool の追加と削除 (deferred tool の読み込み、遅れて来た MCP の tool。`toolAdditions` / `toolRemovals`)
  - attachment を畳み込んだ本文 (`foldedAttachmentSpans` を持つ)
  - 次の user メッセージで消える一時の system (`clear_at:"next_user_message"`)
- 有効になる条件は env `CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM` か、モデルの能力表の `mid_conv_system`。API が 400 / 422 で拒んだら system を抜いて 1 回送り直し、その会話では `/clear` か `/compact` まで止める (`[mid-conv-system] ...` の警告)
- 観測 (tap に入った本文の構造): system の message を含むリクエストは opus-5-5 で 54/54、fable-5-1 で 6/6、sonnet-5-5 で 3/4、haiku-4-5 で 0/88。キーの組は `{content, role}` 268 件、`{content, output_config, role}` 54 件。モデルの能力で分かれているという読みと合う
- どの attachment が畳み込みの対象になるか (畳み込み関数の呼び手) は追っていない

### 2026-10-21: C2. subagent で重複判定がほぼ外れる理由

コード: 索引 (`D3`) は書き込み先のファイルごとに持つ。subagent の記録は `agentId` から subagent の jsonl を選び、その jsonl の末尾を読んで索引を作る。transcript への書き込み (`dedup-transcript` の経路) も sidechain なら同じ subagent の索引に `noteLine` する。つまり索引の作り方は main と subagent で対称で、ファイルの選び違いは原因ではない。

`noteLine` は user / assistant の行を「同じ role が続き、user なら常に、assistant なら同じ `message.id` なら」1 つの run にまとめ、run の途中の各時点の `{"content":[...],"role":...}` の sha256 を入れる。wire の message と一致するのは、transcript の連続した行の内容がそのまま wire の 1 message になっている時だけ。

実測 (haiku の subagent を 1 つ生やす `claude -p` を 1 回。サンプル 1 本なので割合は主張しない): subagent の api-request 2 件の tail 3 個はすべて blob、main の 3 件の tail 5 個は 2 個が transcript から解決した (どちらも並列 tool を含まない assistant)。外れた message を transcript の行と比べると、外れ方は 3 種類あった。

1. 最初の user message: transcript は prompt の文字列 1 つだが、wire では送信時に足される文脈を含む text ブロックの列になる (subagent 7 ブロック、main 9 ブロック)。毎回外れる
2. tool_result: wire の tool_result には送信時に text が足されている (main: text ブロックが 1 つ増え、既存の text も 1 文字長い。subagent: 2 つ目の tool_result が transcript の 9 文字に対し wire では 1978 文字)
3. 並列の tool 呼び出し: transcript は `assistant(thinking) → assistant(text) → assistant(tool_use 1) → user(tool_result 1) → assistant(tool_use 2) → user(tool_result 2)` と交互に書かれ、run が role の切り替わりで切れる。wire はこれを assistant 1 つ (4 ブロック) と user 1 つ (tool_result 2 つ) にまとめるので、どちらも一致しない

subagent の会話は「指示を受けて tool を何度も (しばしば並列で) 回す」形が大半なので、2 と 3 に当たる message が多く、ほぼ全件が blob になる (前回の abark 139/141 件)。main の assistant の blob (前回 258 件) も 3 の並列呼び出しで説明できる。この割合の説明は実測 1 本からの推定で、件数の内訳は取っていない。

### 2026-10-21: C3. `user_identity` の出どころ

2.1.304 のコード (auto mode classifier の context を組み立てる箇所の `user_identity: await <2 秒の timeout>(zbr(), 2000, null)`):

```
async function zbr(){if(Z())return null;let e=a.GITHUB_ACTOR??a.USER??a.USERNAME,n=e===void 0?await S0e():null;return(e??(n?tt(n,"@"):null))?.replace(Tpr,"").slice(0,64)||null}
```

- 環境変数 `GITHUB_ACTOR`、`USER`、`USERNAME` の順に最初にあるもの。どれも無ければ git の user.email (`S0e()` = `gitEmail()`) の `@` より前
- `[^a-zA-Z0-9._-]` を除いて 64 文字で切る。2 秒で取れなければ null
- kawaz の環境で値が OS のユーザ名と同じだったのは `USER` から来たため。git の設定は `USER` が無い時だけ使われる

### 2026-10-21: 予定外だった操作と後始末

- tap は unit の全交換を流すので、購読ファイル 3 本 (最大のもので 116 MB) に稼働中のセッションの本文が入った。集計は構造だけで行い、一時ディレクトリごと削除した
- 5.5 MB / 9.5 MB の prompt で 400 (prompt too long) を 13 回、max_tokens 1 の小さいリクエストを 71 回、haiku の通常の `claude -p` を 2 回 (うち 1 回は subagent を生やす)、`ns-personal` の unstable に流した。どれも `claude-kawazzz` 経由
- 一時の `CLAUDE_CONFIG_DIR` (projects 配下の jsonl を含む) と取得物はすべて削除した

## 暫定的な結論

- tap の request_body は gateway が受けた形で、Claude Code の api-request 記録と同じものを表す。3/3 件で `requestDigest` まで一致し、差は `metadata`・`stream`・billing ブロック・`betas` (ヘッダ) だけ。gateway の加工 (ttl の 1h、thinking の display、model の解決) は tap にも記録にも載らない。`claude -p` は oneshot 扱いで ttl を足されない
- tap の response_body から usage が取れ、`cache_ttl_secs` と stats の 5m / 1h 内訳と整合した。キャッシュの検証 (DR-0024) は「tap の request + response + `cache_ttl_secs`」で回せる
- tap の限界は、`max_body` の上限ではなく次の 4 点にある
  - 既定の 64 KiB では実在のリクエストがすべて切れる (`max_body` を明示すれば 64 MiB まで全量が取れる)
  - 本文の購読中は 1 本ごとに本文の 1〜2 倍のコピーが unit に乗る (gateway 自身の約 5 倍に上乗せ)。response を終端まで溜める分は、実在の応答が数十 KB なので小さい
  - 止まったまま繋がっている購読者は、最大 64 件分の本文を unit に持たせ続け、読み始めるまで切断されない
  - ns で絞れず unit の全交換が流れる。upstream へ送った形は取れない
- DP-Q1 への示唆: 「数分だけ全量を取って突き合わせる」調査は、`max_body` を明示した `timeout <期間> curl -N …` で今回まかなえた (c の始め方で不足は出なかった)。DR-0038 が tap に足す価値として残るのは、ns で絞ること、upstream へ送った形 (DR-0038 未決 2)、ファイルの権限と 48 時間の片付け、容量で止めること。DR-0038 の背景と「採らなかった案」にある「response を終端まで溜めるので並列の数だけメモリに乗る」は実測では主因でなく、メモリで効くのは request のコピーと、止まった購読者が ring に残す 64 件のほう。DR-0038 を進めるなら、その根拠の書き方を直す必要がある
- tap の `ts` が Unix 秒で、MANUAL の約束 (Unix ミリ秒) と例に反している

## 不明点

- 1h 側について、stats の内訳と tap の usage を件ごとに突き合わせていない (自分のリクエストは oneshot で 1h にならず、main の 1h は稼働中のセッションとしか取れなかった)
- RSS の数字は各条件 1 回で、同じ unit に稼働中のトラフィックが流れている。tap の上乗せを本文の 1.1〜1.75 倍と幅で書いたのはそのため
- 10 MB を超える 1 本 (`claude -p` の stdin 上限を超える大きさ) の取り込みは試していない
- C1 で、どの attachment が `api_system` に畳み込まれるか
- C2 の外れ方 3 種の件数の内訳 (多数の subagent の jsonl で数えていない)

## 関連

- [2026-10-21-claude-code-api-request-records.md](2026-10-21-claude-code-api-request-records.md) — 直前の調査 (記録の構造と復元、不明点の出どころ)
- [DR-0017](../decisions/DR-0017-debug-tap-endpoint.md) — tap
- [DR-0038](../decisions/DR-0038-timed-request-dump.md) — 一時ダンプ (DP-Q1)
- [DR-0024](../decisions/DR-0024-cache-strategy-and-keepalive.md) — prompt cache の戦略 (`cache.rs` の `apply` / `apply_one_hour`、`session/origin.rs`)
- `crates/llm-gateway/src/tap.rs`、`crates/llm-gateway/src/exchange.rs` (`BodyObservation`)、`crates/llm-gateway-server/src/lib.rs` (`tap` / `tap_options` / `forward`)
