# Claude Code 2.1.300+ の api-request 記録 (構造・サイズ・送信経路・gateway との突き合わせ)

- Date: 2026-10-21
- Status: Concluded (不明点は末尾に列挙)

## 動機

Claude Code 2.1.300 の changelog に「Changed local session transcripts to also record each API request's system prompt, tools and parameters」(共有したトランスクリプトでリクエストを正確に再現できるようにする) という項目がある。これにより、セッション jsonl に `api-request` / `api-request-shape` / `api-request-blob` の 3 種のレコードが加わった。次の 5 点を確かめる。

- 3 種がどう紐づいているか
- ファイルサイズのうち何割を占めるか
- 外部に送られるか
- 識別子になる値を含むか
- 止められるか、読み戻されてリクエストの中身や挙動を変えるか

あわせて、記録と llm-gateway を実際に流れたリクエストを突き合わせ、記録がどこまで忠実かを調べる。

## 調査範囲

対象は次のとおり。

- `~/.claude-personal/projects/` 配下の jsonl。2.1.301 のバイナリより新しい 160 本を走査し、そのうち記録を含む 49 本 (メイン 8 本、subagent / workflow 41 本)
- Claude Code のバイナリ。2.1.304 を主に読み、2.1.301〜2.1.303 と比べた。中身は bun bytecode に混じった JS 文字列をバイト列として読んだ
- llm-gateway の unstable ログ・stats・`crates/llm-gateway/src/cache.rs`

prompt や thinking の本文は扱わない (構造とサイズだけを見る)。セッション ID は先頭 8 桁で示す。

## 調査メモ

### 2026-10-21: 1. 構造と役割

| レコード | キー | 中身 | dedupe の単位 |
|---|---|---|---|
| `api-request-shape` | `shapeHash` | `shape{system[], tools[]}` (cache_control を除去) | ファイル内で shapeHash ごとに 1 回 |
| `api-request-blob` | `hash` | `message{role, content[]}` 1 件 (cache_control を除去) | hash ごとに 1 回。同じ内容の user/assistant レコードがトランスクリプトにあれば書かない |
| `api-request` | `id` (UUID v4) | `shapeHash`、`params`、`base`/`keep`/`tail[]`、`cacheControl[{path,value}]`、`requestDigest` | 毎リクエスト 1 行 (リトライも 1 行ずつ) |

messages は `(base が指す api-request の messages)[0:keep] + resolve(tail[])` で復元する。`resolve` は、blob の hash か、トランスクリプトの user/assistant レコードから計算した hash で引く。前回リクエストへの連鎖はメモリ上にだけあり、`agentId ?? sessionId` ごとに持つ (上限 64 本)。そのため resume 直後は `keep:0` から始まる。

ハッシュの計算方法 (2.1.304 のコード):

- 正規化 JSON (`yde`) はキーをソートし、`undefined` を落とす。`omitCacheControl` を指定すると `cache_control` キーも除く
- `shapeHash`: `{system, tools}` の sha256。cache_control は除く。system からは `x-anthropic-billing-header:` で始まるブロックを除く (`Tjs`)
- blob の `hash` と `tail[]`: message の sha256。cache_control は除く。string の content は `[{type:"text",text}]` に直してから計算する
- `requestDigest`: 送信 body 全体の sha256。cache_control は含む。トップレベルから除くキーは次のとおり

```
var _de=new Set(["metadata","stream","thread","diagnostics","safeguards"]);   // 2.1.304
var _de=new Set(["metadata","stream","thread","diagnostics"]);                // 2.1.301〜2.1.303
```

その他の欄:

- `withheld`: `CLAUDE_CODE_EXTRA_BODY` で足したキーが params と同じ値のとき、そのキー名を並べる。値が違う場合は、そのリクエスト自体を記録しない
- `cut` / `cutRequestDigest`: アカウントメモリを切り取ったときに付く
- 実データには `withheld` も `cut` も現れなかった

検証として、shape + params + base/keep/tail + blob/トランスクリプト + cacheControl の path からボディを組み立て直し、`requestDigest` を再計算した。9 ファイル・420 リクエストのすべてで記録値と一致した (不一致 0)。内訳は f39d162b 151、e9f6d697 177、5a3754c0 115、66d88310 47、b551b475 16、8aac0fa0 1、subagent 3 本で 84。

### 2026-10-21: 2. サイズ

記録を含むのは 2.1.300 以降の jsonl 49 本で、2.1.299 の 110 本には 1 件も無い。49 本の合計 117.15MB のうち、3 種が 48.7% を占める (api-request 25.25MB / blob 29.88MB / shape 1.96MB)。ただし、2.1.300 より前の内容と混在しているファイルも含む。

| ファイル | 版 x 件数 | 全体 MB | api-req MB | shape MB | blob MB | 全体比 | 記録開始以降の比 | api-req 中の safeguards |
|---|---|---|---|---|---|---|---|---|
| 8aac0fa0 (main) | 2.1.304 x1 | 0.50 | 0.00 | 0.040 | 0.13 | 33.6% | 72.1% | 0% |
| 980dabe6 (main) | 2.1.304 x7 | 0.58 | 0.01 | 0.040 | 0.14 | 32.6% | 61.0% | 0% |
| b551b475 (main) | 2.1.302 x16 | 1.20 | 0.08 | 0.087 | 0.20 | 30.4% | 57.8% | 69% |
| f39d162b (main) | 2.1.304 x151 | 2.82 | 0.22 | 0.132 | 0.34 | 24.4% | 27.0% | 0% |
| 66d88310 (main) | 2.1.300 x47 | 6.06 | 2.16 | 0.087 | 0.61 | 47.2% | 80.2% | 95% |
| e9f6d697 (main) | 2.1.304 x177 | 9.48 | 0.29 | 0.087 | 1.20 | 16.7% | 22.1% | 0% |
| 5a3754c0 (main) | 2.1.301 x115 | 26.47 | 19.30 | 0.086 | 0.73 | 76.0% | 94.9% | 99% |
| agent-abark-research (subagent) | 2.1.302 x70 | 6.55 | 0.12 | 0.055 | 5.48 | 86.3% | 89.8% | 11% |
| agent-athinking-stats2 (subagent) | 2.1.304 x12 | 1.82 | 0.02 | 0.032 | 1.26 | 71.5% | 83.5% | 0% |
| agent-ae51352f (workflow) | 2.1.304 x2 | 0.41 | 0.00 | 0.028 | 0.12 | 37.6% | 96.3% | 0% |

- 2.1.300〜2.1.303 で膨らんだ原因は `params.safeguards` (auto mode classifier の context)。5a3754c0 では api-request が 1 行 140〜320KB あり、そのうち `classifier_context.prior_turn_context` が 133〜259KB を占める。2.1.304 で safeguards は記録から外れ、api-request は 1 行 1〜2KB になった。changelog 2.1.304 の「the safety classifier's context is no longer stored with every request」と一致する
- 2.1.304 でサイズを占めるのは blob。メインセッションでは、メッセージ hash の 60〜75% がトランスクリプトから解決できる (e9f6d697 は 746 件中 455 件、f39d162b は 306 件中 230 件)。subagent ではほぼ全件が blob になる (abark は 141 件中 139 件、thinking-stats2 は 25 件中 23 件)。同じ会話がトランスクリプトと blob に二重に書かれ、ファイルはおよそ 2 倍になる
- shape は 1 セッションあたり 30〜130KB で、tools の構成が変わるたびに 1 本増える

### 2026-10-21: 3. 外部に出るか

送信する箇所は見つからなかった。根拠は 2.1.304 のコード。

| 経路 | 扱い | 根拠 |
|---|---|---|
| 記録するかどうかの条件 | remote にミラーされるセッション (CCR v2 の internalEventWriter、または remote ingress) では記録しない | `Lvt({canRecord:()=>!za()&&!ga().isRemotelyMirrored(),...})` |
| 書き込みの振り分け | 3 種は `route-by-agent` で、ローカルファイルへの `enqueueWrite` だけ。remote に送る `persistToRemote` は `dedup-transcript` (user/assistant 等) の経路でしか呼ばれない | `rri` 表の `"api-request":"route-by-agent"` |
| SessionMirror (CCR) | 書き込みキューの flush で、ミラー対象から外している | `if(b=V.storageV5,h+=_e,!Q.type.startsWith("api-request"))H?.push(Q)` |
| /bug・/feedback (POST `/api/claude_cli_feedback`) | jsonl の末尾 4MB と最近のセッションを `I$e()` に通し、api-request 系の行を削除してから送る | `if(typeof h==="string"&&h.startsWith(aTt))return;` (`aTt="api-request"`) |
| /share | /bug と同じダイアログ。`CLAUDE_CODE_ENABLE_SHARE_BUNDLE` か flag `tengu_humming_wave` が有効な場合はローカルに zip を作るだけで、そこでも同じ削除をしたうえで、api-request を含む行の 64 桁 hex を `[REDACTED]` に置き換える | `function cs(e,s){return s.includes("api-request")?e.replace(an,"[REDACTED]"):e}` |
| Remote Control のバックフィル | `getBackfillMessages` はメモリ上のメッセージ (`R.getSnapshot()`) から作り、jsonl の行は読まない | bridge の初期化コード |
| telemetry | 記録できなかった理由の文字列だけを送る | `p("transcript_request_replay",Bt)` |

`autoUploadSessions` は「Mirror local sessions to claude.ai as view-only」と説明されている設定で、この環境では未設定だった。実装の経路は追い切れていない (不明点に記載)。

### 2026-10-21: 4. signature と識別子

- blob に入っている `signature` は thinking block の署名だけで、283 件すべてが assistant レコードの signature と一致した (欠けは 0)。blob のブロック種別は text / tool_use / tool_result / thinking だけ
- ハッシュ類 (`shapeHash` / `hash` / `tail[]` / `requestDigest`) は鍵を使わない素の sha256 で、HMAC ではない。再計算で値が一致したことで確定した。中身から決まる値なので、アカウントや機体の識別子にはならない (短いメッセージなら総当たりで推測される余地はある)
- レコードに含まれる ID は `sessionId`、`agentId`、`id`/`base` (リクエストごとのランダム UUID)、`timestamp`、`version`、`querySource`。assistant レコードの側には `requestRef` (= api-request の `id`) が付く
- 設計上記録しないもの (8 節で、実際に送られたリクエストと比べて確認済み):
  - `metadata.user_id` (JSON 文字列で `{device_id(64 桁), account_uuid, session_id}`。device_id は機体の識別子)
  - billing header ブロック
  - `stream`
  - HTTP ヘッダ全般
- 実データの全値 (blob の本文を除く) を走査した結果、email、`user_id` / `device_id` / `account_uuid` / `organization_uuid` / `org_id` / `installation`、billing header はいずれも 0 件。UUID が現れるのは sessionId / id / base だけ
- 機体に近い情報を持つ欄は、2.1.300〜2.1.303 の `classifier_context` にだけある。`user_identity` (値は OS ユーザ名と同じ文字列)、`home_dir`、`live_cwd`、`rule_roots`、`trusted_directories`、`git_state{branch,cwd,default_branch,root,status,visibility}`、`platform`、`permission_mode`、`auto_mode` (autoMode 設定の本文 10KB)、`rules`、`prior_turn_context` の各欄。installation id / device id / account / org id は無い。2.1.304 の記録 (1015 件) には classifier_context 自体が無い

### 2026-10-21: 5. 制御と resume

- 記録を止めるのは env `CLAUDE_CODE_ELEGANT_MEADOW=0` (triBool で、`0/false/no/off` が false)。未設定の場合は GrowthBook の flag `tengu_elegant_meadow` (既定 true) に従う。実機で確認した: 隔離した環境で、既定では api-request 2 件と shape / blob が書かれ、`=0` では 3 種とも 0 件だった
- 暗黙に記録されない条件:
  - セッションの永続化が無効 (`za()`)
  - querySource が main / sdk / subagent 以外 (compaction、タイトル生成、classifier 等)
  - remote ミラー中、storageV5、fork したセッション (`fork-context-ref` を持つ)
  - shape が 2MB を超える
  - `CLAUDE_CODE_EXTRA_BODY` が params と食い違う
- 間引く設定は無い
- `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC` は、非対話・sdkUrl なし・`CLAUDE_CODE_REMOTE` でない・env が true、のすべてを満たすときだけ有効になる (`ui()`)。対話 CLI では効かない。GC の rewrite 計画で 3 種は `boundary-cleared` に分類されているので、compact boundary より前のものは消える見込みだが、分類名からの推定で未検証。発火の閾値は、ファイル 5MB 以上かつ前回から 20MB 追記されたとき。policySettings で強制的に off にできる
- resume 後に初めて記録するとき、`pri()` が jsonl の末尾 50MB を読み、`D3.noteLine` で重複判定用の hash 集合を作り直す。会話の復元 (`loadTranscriptFile`) は params / tail / keep を参照しない。したがって 3 種を消しても resume は壊れず、次のリクエストで shape と blob を書き直すだけになる見込み。3 種を消した状態での resume 実機確認はしていない

### 2026-10-21: 6. 使い道 (実データの例)

1. リクエストの再現: 420/420 件で digest が一致した。8 節で、実際に送られた body と 1 バイト単位で一致することも確認した
2. tool 定義の版の追跡: f39d162b は shape が 3 本ある。system の 2 ブロック (57 / 7008 文字) は同一のまま、tools が 14 → 15 (+PushNotification) → 16 (+SendMessage) と増えた時点を特定できる
3. cache_control の配置: 1329 リクエスト中 1276 件は `system[0]`・`system[1]`・末尾付近の messages 2 箇所の計 4 点、53 件は 3 点。値はすべて `{"type":"ephemeral"}` (ttl なし)。tools に付いたものは 0 件
4. params の追跡: model / effort / max_tokens / thinking / context_management / betas (11〜14 個) をリクエスト単位で追える。model 名は gateway の別名 (fable / sonnet / gpt-6-sol) のまま残る
5. 送信時の形の観察: blob には `role:"system"` のメッセージが 134 ブロック、tool_use の `caller` 欄が 194 件ある。どちらもトランスクリプトの形とは異なる
6. classifier_context の中身の確認: 2.1.300〜2.1.303 の記録 189 件で見える。2.1.304 以降は記録されないので、この用途はなくなった
7. キャッシュの接頭辞: `keep>0` が 96%、tail の中央値は 2 で、前回リクエストと共通する先頭部分の長さがそのまま読める。8 節で、gateway の usage と相関 0.999 で整合した

### 2026-10-21: 7. 書くだけか、読み戻すか

結論は「書くだけ」で、API に送る内容は変わらない。

- 2.1.304 のコード領域で読み手を全部列挙した。`shapeHash` は 8 箇所あり、すべて記録処理の中 (生成する `Nvt`、`appendRequestReplayRecords` での oversize 判定と既出判定、重複判定の索引 `D3.noteLine`)。`requestDigest` は 3 箇所で、計算と書き込みだけ (`Dvt` のエラー文と `Nvt`)。記録処理の外でこれらを比べて何かを省いたり分岐したりする箇所は無い
- 記録処理の呼び出しは送信経路の `ku=Nvt({params:zr,...})` の 1 箇所だけで、params をコピーして読むだけ (書き換えない)。戻り値の api-request id は assistant レコードの `requestRef` に入るだけで、`requestRef` を読むコードは無い (3 箇所の出現はすべてオブジェクトを組み立てる箇所)。実データでは f39d162b の assistant 232 件すべてに `requestRef` があり、すべて api-request の id と一致した
- 副作用は CPU とディスクだけ。送るたびに sha256 を計算する (共通の接頭辞はメモ化)。resume 後に初めて記録するとき、jsonl の末尾 50MB を読む
- 実機でも確認した。隔離環境で、記録あり (既定) と記録なし (`=0`) をそれぞれ 1 回走らせ、ローカルのスタブで HTTP リクエストを捕まえた。次の項目はすべて同一だった
  - リクエストの順序 (HEAD `/api/hello` → GET `/v1/models` → POST `/v1/messages?beta=true` x2)
  - ヘッダのキー集合と値 (auth / session-id / host / content-length を除く)
  - `anthropic-beta`
  - body の長さ (84550 / 127142)
  - cache_control の位置

  違ったのは、実行ごとに変わる値 (一時ディレクトリ名、metadata の device_id / session_id) だけ
- 「再現」をする側の機能は無い。名前に replay を含む識別子を全部洗ったが、`--replay-user-messages` (SDK 入力の echo)、`tether-replay` (サーバ側スレッド継続の再送で、`fingerprint_mismatch` を扱う。jsonl は読まない)、hooks / artifact の replay と、どれも別の機能だった。fork したセッションでは記録自体が止まる。api-request 系を読んでリクエストを組み立て直す機能 (`--replay`、cloud fork、`/branch` の差分) は見つからなかった。changelog の「共有トランスクリプトで再現」は、jsonl ファイルを人に渡して外部で組み立て直す使い方を想定していると読める (推測)

### 2026-10-21: 8. llm-gateway を流れた実リクエストとの突き合わせ

(a) 指示どおりの形で gateway に 1 本流した

- 実行: `claude -p --model claude-haiku-4-5-20251001 'Reply with the single word: ok'`。exit 0 で `ok` が返った。`ANTHROPIC_BASE_URL` は settings の env が優先されたが、リクエストは unstable のログに `req=11515` として現れた
- 時刻の対応: api-request の timestamp が 15:50:56.860Z、gateway の `received request body` が 15:50:56.871Z (11ms 後)。model は `claude-haiku-4-5-20251001`
- サイズの対応: gateway の `body_bytes` は 197709。記録から復元した body (digest 一致) は 197631 で、これは betas を body に含めた形。(b) で特定した差分を当てると、197631 − 224 (betas の欄) + 188 (`metadata`) + 14 (`stream`) + 100 (billing ブロック) = 197709 で、ぴったり一致した

(b) 記録から何が落ちているか (隔離環境で、ローカルのスタブを使って実際の送信を捕まえた)

- 実際の body から `metadata`・`stream`・billing ブロックを除き、`anthropic-beta` ヘッダの値を `betas` として足すと、記録から復元した body と正規化 JSON で完全に一致した。`requestDigest` も一致した (2 本とも)。これで digest の計算元が確定する: 実際の body から {metadata, stream, (thread, diagnostics), billing ブロック} を除き、betas を足したもの
- betas は body に入らず、HTTP ヘッダ `anthropic-beta` で送られる。記録には `params.betas` として残る
- 記録に無いヘッダ: `x-claude-code-session-id`、`user-agent: claude-cli/2.1.304 (external, sdk-cli)`、`x-app: cli`、`x-stainless-*` (OS / arch / runtime / retry-count / timeout)、`anthropic-version`、`anthropic-dangerous-direct-browser-access`、`Authorization`
- cache_control の system 側の位置が 1 つずれる。実際の body では billing ブロックが `system[0]` に入るため、送信時の位置は `system[1]` と `[2]`。記録の path は billing を除いた配列での位置なので `system[0]` と `[1]` になる。messages 側の位置は一致する
- gateway が中身を書き換える。Claude Code が送る cache_control は `{"type":"ephemeral"}` で ttl が無い (記録と送信の両方で確認)。しかし stats では main が `input.cache_creation.ephemeral_1h` に計上されている。これは gateway の `crates/llm-gateway/src/cache.rs` の `apply_one_hour` (DR-0024) が main に `ttl:"1h"` を付けるため。つまり記録が表すのは「Claude Code が gateway に送ったもの」で、upstream に届くものではない
- 記録の件数は成功した通信の件数と一致しない。401 が 10 回返った実行では、リトライのたびに api-request が書かれた (10 件、`keep:1 tail:0`、digest はすべて同じ)。記録は送信の前に書かれ、失敗しても残る。`requestDigest` はリクエストごとに一意ではない

(c) cacheControl と usage / stats の整合

- haiku の 1 本: breakpoint は 3 点 (`system[0]`、`system[1]`、`messages[0].content[10]`)。usage は cache_creation 59494 / read 0 / input 10 / output 995 で、stats (`stats/2026-10-21.127-0-0-1-11301.json` の claude-haiku `oneshot`) と完全に一致した。ttl が無いので 5 分の枠に計上されている
- 既存セッションでは、`requestRef` で api-request と usage を結び、cache_read の予測値を作って比べた。予測は「前回リクエストの breakpoint のうち、今回の `keep` の範囲に収まる最大のもの」までのバイト数。実際の cache_read tokens との相関は f39d162b (151 件) で 0.9993 (tokens/byte の中央値 0.35)、e9f6d697 (177 件) で 0.92
- 各リクエストで cache_read(n) = cache_read(n−1) + cache_creation(n−1) が成り立っていた (例: 62038 → +899 → 62937 → +20895 → 83832)。最後の breakpoint は毎回末尾の block にあり、キャッシュされない input は 2〜4 tokens
- 例外が 2 件ある。shape が変わった直後なのに cache_read が 15.8 万 tokens あり、予測と合わない (不明点に記載)

### 2026-10-21: 予定外だった操作

- ローカルのスタブに向けて `ANTHROPIC_BASE_URL` を env で渡したが、`~/.claude-personal/settings.json` の `env.ANTHROPIC_BASE_URL` (https の gateway の `/ns-personal`) が優先された。その結果、ダミー token を付けた haiku リクエストが gateway の namespace `personal` に 10 回送られ、すべて 401 になった。実害は無いが、gateway のログに 401 が 10 件残っている。以降は `CLAUDE_CONFIG_DIR` を一時ディレクトリに切り替えて隔離した
- 次の 2 本の jsonl を projects 配下に残している (消していない)
  - 指示どおりの gateway 実行: `~/.claude-personal/projects/-private-var-folders-…-tmp-xkBIxoWAMz/1dc0637b-….jsonl`
  - 上の 401 x10: `~/.claude-personal/projects/-private-var-folders-…-tmp-71rWj5XLV2/88a3c03e-….jsonl`
- 捕まえたリクエストと一時 config は削除した

## 暫定的な結論

- 3 種のレコードは、送信 body を内容アドレス (sha256) で差分記録したもの。記録だけから body を組み立て直せ、実際の送信と 1 バイト単位で一致する。落ちているのは `metadata`・`stream`・billing ブロックと HTTP ヘッダで、betas はヘッダ `anthropic-beta` の値として params に入っている
- 書くだけで、読み戻してリクエストや挙動を変える箇所は無い。外部へ送る経路も無い (feedback / share / CCR ミラーはいずれも除外している)
- サイズの主因は 2.1.300〜2.1.303 の safeguards (2.1.304 で解消済み) と、subagent での blob の二重書き
- gateway の観点では、記録は「Claude Code が gateway に送ったもの」の忠実な写しで、cache の ttl 書き換えなど gateway の加工は含まない。`requestRef` で usage と結べば、キャッシュの効き方を breakpoint 単位で検証できる

## 不明点

- subagent で、トランスクリプトとの重複判定がほぼ当たらない理由 (メインでも assistant の blob が 258 件ある)
- `role:"system"` のメッセージがどこで作られるか
- tools が変わった直後でも cache_read が大きい 2 件の理由
- `autoUploadSessions` / Remote Control を有効にしたとき、remote ミラーの状態になって記録が止まるか
- `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC` で 3 種が実際に消えるか
- 3 種を削除したあとの resume (実機では未実施)
- `user_identity` の値の出どころ (OS のユーザ名か git の設定か)
- `cut` / `withheld` の実例 (該当データ無し)
- 送信を捕まえた実行は一時 config で行った。kawaz の設定 (auto mode、プラグイン等) のもとでも同じ規則に従うかは、(a) のサイズ一致 1 件でしか確かめていない

## 関連

- DR-0024 (gateway の prompt cache の扱い。`crates/llm-gateway/src/cache.rs`)
- Claude Code changelog 2.1.300 / 2.1.304
