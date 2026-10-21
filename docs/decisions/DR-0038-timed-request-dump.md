# DR-0038: リクエストと応答の本文を、時限と容量の上限つきで一時的にダンプする `dump start|stop|list|prune`

- Status: Accepted (kawaz が 2026-10-21 に着手を裁定し、同日に設計の一般形を claude-rules-personal の reference `proxy-dump-design` へ切り出した。本 DR はその reference を正本とし、llm-gateway 固有の決定だけを持つ)。決定 1〜13 と「影響」の段分けの段 1〜4 は実装済み
- Date: 2026-10-21

## 背景

Claude Code の jsonl 記録 (api-request / shape / blob) と gateway を実際に流れた本文を突き合わせようとして、gateway 側に本文を保存する手段が無いことに当たった (2026-10-21)。DR-0024 の prompt cache の検証や「upstream が何を返したか」の調査でも、毎回同じ所で困る。tap (`GET /llm-gateway/tap`) は繋いでいる間だけ流れる揮発的な口で、本文は既定 64 KiB で切り詰められ、応答は終端まで溜めてから配る (実測は `docs/research/2026-10-21-tap-body-capture-and-limits.md`)。keepalive の控え (DR-0027) は送り直すための本文で、調査のために残すものではない。

kawaz の要件 (2026-10-21) は 3 つ。常時ではなく一時的に有効化できること、溜まり事故を防ぐため外部から on/off でき必ず時限で切れること、片付けを忘れるので 48 時間より古いダンプは勝手に消えること。

この要件を満たす proxy の一時ダンプの設計 (目的と残すもの、系列と `dump_id`、試行、一致条件、状態と停止理由、設定キーと容量の判定、クリーンアップ、保存先の権限、ファイル構成と index.jsonl のイベント、start / end / list / prune、follow、labels) は **claude-rules-personal の reference `proxy-dump-design`** (`model` / `capacity` / `files` / `api` の 4 本) が正本で、本 DR には再掲しない。本 DR が決めるのは、その一般形を llm-gateway に当てはめた時に決まる固有の点だけ。各項の末尾の括弧は、reference のどの本に対する固有化か。

## 決定

### 1. 経路は制御 socket で、CLI は最上位の `dump start|stop|list|prune` (api)

```
llm-gateway dump start --ns <ns> [--timeout <duration>] [--max-bytes <n>] [--store raw] [--label <k>=<v>]... [--follow [--embed bodies [--no-persist]]] [--unit <unit>]
llm-gateway dump stop  <dump_id> [--unit <unit>]
llm-gateway dump list  [<selector>]... [--unit <unit>]
llm-gateway dump prune --dump-id <dump_id>|<selector>... [--unit <unit>]
```

- 書き込み系を HTTP に置かない構成なので、reference の `POST dump/start` 等は DR-0032 決定 2 の経路 (CLI → 監督者 `supervisor.sock` → unit の制御口 `daemon/control/<unit>.sock`) で同じ意味を持つ。ダンプは会話の本文をディスクへ書く操作で、外 (Caddy 越し) から届く面に置かない理由は DR-0032 が reload を HTTP に置かない理由と同じ。監督者が居なければ `supervisor_not_running` で断り (DR-0028 決定 3)、制御口が dump を知らない版の unit には `ok: false` を返す (DR-0032 の「版の混在」)
- 最上位に置くのは、`daemon` が「この instance のプロセス操作」(DR-0028 決定 1) で、ダンプは unit を上げ下げも読み直しもせず、`usage` / `stats` と同じ「走っている unit に聞く・頼む」系だから。unit の指し方は DR-0028 決定 6 (`--unit`、登録が 1 つならそれ、複数で未指定なら `unit_required`)
- `status` は持たず `list` に吸収する。`list` は選択子が無ければその unit の系列を全部並べ、動いているものだけ見たければ選択子 `state=active`。選択子は reference の labels の形 (`owner=kawaz` / `pr!=1234` / `owner`) に `state=active|stopped` を足したもの
- labels のキーと値は 1〜63 文字、英数字と `-` `_` `.`、先頭と末尾は英数字 (統括の裁定)。読めない labels・選択子は `bad_request`
- `dump` は子を持つレベルなので、引数なしと `--help` はテキストの help。`start` は必須の `--ns` が無ければ help、`stop` は `<dump_id>` が無ければ help、`prune` は `--dump-id` も選択子も無ければ help (無指定の全消しは reference のとおり持たない)。`list` は省略可能な引数だけなので実行する
- このリポの CLI は completion 定義を持たない (DR-0030 と同じ)。揃えるのは `--help` の文面と実装の 2 者

### 2. 一致条件は `--ns` だけで、必須 (model)

reference の「面の指定を必須」を ns に当てる。1 回の `start` は 1 つの ns だけを対象にし、他の軸 (model / route / credential 等) は今は持たない。指した ns が走っている設定に無ければ `unknown_namespace`、ns の名前がディレクトリ名にできなければ (DR-0037 §12 と同じ制限: 英数字と `.` `_` `-`、`.` で始まらない) `invalid_namespace` で断る。

### 3. 設定は `[dump]` テーブルに歯止めと既定値だけ (capacity)

reference の設定キーから `dump_` 接頭辞を外した名前で `[dump]` に置く。どのキーも既定値を持つので、テーブルごと書かなくてよい。on / off や対象 ns のような「ダンプを始める」指定は設定に持たない (書いたものは消し忘れでそのまま常設になり、要件の「一時的に」と逆になる)。

| キー | 既定値 | 対応する reference のキー |
|---|---|---|
| `default_timeout` | `5m` | `dump_default_timeout` |
| `default_max_bytes` | `209715200` (200 MiB) | `dump_default_max_bytes` |
| `max_timeout` | `24h` | `dump_max_timeout` |
| `max_total_bytes` | `2147483648` (2 GiB) | `dump_max_total_bytes` |
| `fs_min_free` | `1073741824` (1 GiB) | `dump_fs_min_free` |
| `cleanup_ttl` | `48h` | `dump_cleanup_ttl` |
| `max_concurrency` | `16` | `dump_max_concurrency` |

- duration は設定の他の欄と同じ書き方 (`10m` / `1h`)、サイズは bytes の整数
- `max_timeout` は `cleanup_ttl` 以下 (reference の制約) で、`llm-gateway check` が検証する。`--timeout` が 0 か `max_timeout` を超えれば `invalid_duration` で断る
- `default_max_bytes` が reference の例 (5 MiB) より大きいのは、Claude Code の 1 本が request で 500 KB 前後〜1 MB 弱 (reference `model` の適用例の実測) あり、並列の worker が重なるため

### 4. 置き場は `<[stats] dir>/dumps/<unit>/<ns>/<dump_id>/`、片付けは `dumps/` 全体を unit の起動時と `dump start` の時に (capacity / files)

reference の `dump/{dump_id}/` を `<[stats] dir>/dumps/<unit>/<ns>/<dump_id>/` に置く。`[stats] dir` は keepalive の控え (DR-0027)、thinking の出所 (DR-0035 §6)、自動の鍵 (DR-0037 §12) が使う「兄弟の unit が共有する走行状態」の置き場で、ダンプも同じ性質を持つ。新しい置き場のキーは足さない。

- `<unit>` を挟むので書き手が unit ごとに分かれ、`.lock` の flock は要らない (DR-0011 の「writer 毎に分けて排他を消す」)。stable と unstable が同じ ns を同時にダンプしても互いのファイルに触らない
- `<ns>` を挟むのは決定 2 の理由 (面の内容が別の面の作業で読まれにくくする)
- 権限は reference のとおりディレクトリ 700、ファイル 600 で、自動の鍵のファイルと同じ閉じ方
- クリーンアップの契機は unit が上がった時と `dump start` を受けた時の 2 つで、定期の timer を足さない。どちらも「ダンプに関心が向いた瞬間」だから、そこで掃けば溜まらない。掃く範囲は `dumps/` の下全体 (他の unit の分も含む)。止まったままの unit は自分の分を掃けないので、どれかの unit が上がれば全部が片付く形にする。古さは系列ごとの `delete_after` (始めた時刻 + 始めた unit の `cleanup_ttl`) で判定し、過ぎたものをディレクトリごと消す。`delete_after` は系列の `index.jsonl` の `start` の行から読み、掃く unit の `cleanup_ttl` を兄弟の unit の系列に当てない (兄弟の設定は違いうる)。`start` の行が読めない系列だけ、`dump_id` (UUIDv7) の時刻部に掃く unit の `cleanup_ttl` を足して代わりにし、log に出す。`max_timeout` ≤ `cleanup_ttl` (決定 3) なので、兄弟 unit が動かしている系列を消すことがない。空になった `<ns>/` と `<unit>/` も消す
- `start` と `list` の答えに系列のディレクトリの絶対パス (`dump_dir`) を載せる。突き合わせる人が置き場を導出し直さずに済む

### 5. `seq` は `exchange::request_span` の通し番号、`{ts}` は Unix ミリ秒 (files)

ファイル名 `{ts}-{seq}` と index.jsonl の `seq` は `exchange::request_span` が振る unit の通し番号で、events (DR-0012) の `seq` と unit の log の `req` と同じ番号。`{ts}` は request を受けた時刻の Unix ミリ秒 (DR-0012 の規約)。index.jsonl の各イベントの `ts` も同じ規約で、`duration_ms` だけが名前に単位を持つ長さ。

### 6. `store` は `raw` だけ (files)

llm-gateway は upstream に圧縮を求めない (クライアントの `accept-encoding` を `egress` で落とし、自分でも付けない) ので、`raw` で保存した body の実態は非圧縮で、読む側に展開の責務は生じない。`--store decoded` は `bad_request` で断る。`store` の欄と `content-encoding` / `body_stored` は reference のとおり json に残す。

### 7. gateway の書き換えは試行ごとの `*.<attempt>.response.json` の `request_rewrites` に書く (files)

記録本体 (`*.request.body`) は受けた形のまま、gateway がその経路へ送る形を決めるまでに加えた書き換えを、試行ごとの `*.<attempt>.response.json` の `request_rewrites` に書く (統括の裁定)。model の別名の解決と経路ごとの名前、cache の戦略と寿命 (DR-0024) は経路ごとに効くので、試行が複数ある 1 本では `*.request.json` 1 つに収まらない。`*.request.json` には置かない。中身は `model: {received, sent}` (送った名前が受けた名前と違う時)、`thinking_display` (受け口が display を上書きした時)、`thinking_as_text` (跨ぎで text にした時)、`cache_strategy`、`cache_ttl_secs`。upstream へ送った本文そのものは残さない (reference `model`)。

### 8. 認証系ヘッダの述語は `gateway.rs` の `is_authorization`、ヘッダは同名を保つ配列 (capacity / files)

gateway は控えに持ち回してはいけないヘッダの述語を既に 1 つ持っている (`is_authorization`: `authorization` / `proxy-authorization` / `x-api-key` / `cookie`、DR-0027 決定 1)。ダンプも同じ述語で落とし、2 つ目の一覧を持たない。受けた形の `Authorization` には ns 認証の token (DR-0030) が入るので、落とす対象はそこ。それ以外のヘッダは全部残す (`anthropic-beta` / `x-claude-code-*` のように突き合わせに要るものを選別しない)。

メタ json のヘッダは `{name, value}` の配列で、同名のヘッダを繋がずに受けた順で保つ (`rewrites` のヘッダの `{op, name, value}` と同じ語彙)。

### 9. 控える位置は受け口と、経路が upstream の本文を読み出した直後 (model)

request は受け口 (`llm-gateway-server` の `forward`、`messages` / `responses` の両方が通る) で、ヘッダを受けて ns 認証 (ヘッダだけで決まる) を通した直後、本文を読む前に `request_headers` を書き、本文を受け取り終えたら `*.request.json` / `*.request.body` / `request_body_end` を書く (書き換えは試行ごとの response 側に載せるので、送る形が決まるのを待たない — 決定 7)。応答の順 (本文が読めない → JSON でない → ns が無い → 認証に断られる) は変えない。response は各経路の `Wire::send` (`AnthropicWire` / `OpenAiWire`) が upstream から本文を読み出した直後に `egress::observe_raw` へ通し、方言の通訳 (`openai::response::translate`)、採用判定 (`ResponseAdmission::admit`)、集め直し (`egress::adapt_response`)、エラー本文への差し替え (`client_error_response`) を経る前のチャンクを、通り過ぎるたびに書く。

- ns 認証に断られた 1 本は書かない (request を受けた、と言えるのは認証を通ってから)
- 本文を受け取りきれなかった・読めなかった 1 本 (JSON として読めない、途中で切れた、64 MiB を超えた) は、`request_body_end` と `*.request.json` に `error: "invalid_json" | "truncated" | "too_large"` を付けて残し、受けた本文 (切れたならそこまで、超えたなら上限まで) を生のまま `*.request.body` に書く。クライアントが何を送ったかが調査の材料だから (統括の裁定)
- gateway が自分で組んだエラーを返した 1 本 (経路が尽きた・モデルが無い等) は、受け口がエラー応答を返す所で `response_headers_sent` (`adopted: null`) を書く
- 認証差し替えの中継 (DR-0030 §2 の passthrough) は本文を読まずに流す口で、対象にしない

### 10. 答えは `{unit, ok, …}` で始め、時刻は Unix ミリ秒 (api)

DR-0032 決定 5 の流儀に揃え、`ok: true` の答えに reference の欄を同じ階層で添える。時刻の欄 (`started_at` / `expires_at` / `delete_after` / `stopped_at` / `fs_checked_at`) は reference の名前のまま、値は DR-0012 の規約で Unix ミリ秒の数値にする (RFC3339 にしない)。

```
llm-gateway dump start --ns lab --timeout 10m --label owner=kawaz --unit unstable
{"unit":"unstable","ok":true,"dump_id":"019a1b2c-3d4e-7f80-9123-456789abcdef","dump_dir":"<stats dir>/dumps/unstable/lab/019a1b2c-…","ns":"lab","timeout_ms":600000,"max_bytes":209715200,"store":"raw","labels":{"owner":"kawaz"},"started_at":1792000000000,"expires_at":1792000600000,"delete_after":1792172800000,"concurrency":{…},"storage":{…}}

llm-gateway dump stop 019a1b2c-3d4e-7f80-9123-456789abcdef --unit unstable
{"unit":"unstable","ok":true,"dump_id":"019a1b2c-…","dump_dir":"…","ns":"lab","state":"stopped","reason":"stop","stopped_at":1792000312000,"bytes_written":1048576}

llm-gateway dump list state=active --unit unstable
{"unit":"unstable","ok":true,"dumps":[{"dump_id":"019a1b2c-…","dump_dir":"…","ns":"lab","state":"active","reason":null,"labels":{"owner":"kawaz"},"started_at":1792000000000,"expires_at":1792000600000,"delete_after":1792172800000,"stopped_at":null,"bytes_written":1048576}]}

llm-gateway dump prune owner=kawaz --unit unstable
{"unit":"unstable","ok":true,"pruned":["019a1b2c-…"]}
```

- `start` の答えは reference の欄 (`dump_id`、適用されたパラメータ、`concurrency`、`storage`) に `ns` と `dump_dir` を足したもの
- `stop` は reference のとおり冪等で、止まっていた系列にはその元の `reason` を返す。答えは `list` の 1 件と同じ形。restart 前の系列 (置き場にディレクトリだけが残る) も止まっている系列として答え、メモリにも置き場にも無い id は `unknown_dump`
- restart 前の系列は `list` に `state: "stopped"` で並ぶ。`reason` は `index.jsonl` の `stop` の行の値、その行が無ければ (止める前に unit が落ちた) `"interrupted"`。labels と時刻は `start` の行から読み、`prune` の対象になる (統括の裁定)
- `list` の 1 件は `start` の答えから `concurrency` / `storage` を除き、`state` / `reason` / `stopped_at` / `bytes_written` を足した形。`prune` は消した `dump_id` を `pruned` に並べる
- 断る時は `ok: false` と `error: {kind, message}`。`kind` は `unknown_namespace` / `invalid_namespace` / `unknown_dump` / `invalid_duration` / `too_many_dumps` (動いている系列が `max_concurrency` に達している) / `bad_request` (`--max-bytes` が 0、`--store` が `raw` 以外、`--no-persist` が follow + embed 以外、labels か選択子が読めない、`prune` に `--dump-id` も選択子も無い、版が dump を知らない) / `dump_dir_unwritable`。CLI が頼む先を決められない時 (`unit_required` 等) や監督者に届かない時 (`supervisor_not_running`) も同じ `{unit, ok: false, error}` の形で、頼む先が決まっていなければ `unit` を付けない、監督者と CLI の側は DR-0032 の表 (`not_running` / `restart_required` / `unreachable` / `no_answer` / `bad_answer` / `unknown_unit` / `supervisor_not_running` / `unit_required` / `no_units`)

### 11. `/llm-gateway/self` と `daemon status` に、動いている間だけ `dump` を出す (api)

溜まり事故を防ぐのが要件の芯で、普段見る `daemon status` に出ていれば止め忘れに気づける。unit は `/llm-gateway/self` に `dump: [{dump_id, ns, expires_at, bytes_written}]` (active の分だけ) を載せ、監督者は DR-0028 決定 5 の経路で各行に写す。1 つも動いていなければ欄ごと出さない。

`/self` は認証の無い口だが、載せるのは id と ns の名前と時刻と大きさまでで、events (DR-0012) が既に流している以上の情報は含まない。本文の置き場 (`dump_dir`) も labels も載せない (それらは制御 socket 越しの `dump list` だけが答える)。

### 12. `--follow` は index.jsonl の行を ndjson で標準出力へ流し、Ctrl-C で止める (api)

`dump start --follow` は、制御 socket 越しに index.jsonl へ追記するイベントをそのまま 1 行 1 JSON で標準出力へ流す (最初の行は `start` イベント)。CLI が Ctrl-C 等で接続を閉じると unit は `disconnected` で止め、時限や上限で止まった時は `stop` の行を流してから閉じる。背圧 (`lagged`) と `--embed bodies` / `--no-persist` の意味は reference のとおり。

### 13. tap を廃止し、本文を控える口はダンプ 1 つにする (model)

tap (DR-0017、`GET /llm-gateway/tap` と `tap::` モジュール、`TapObservation`) を取り除き、本文を控える位置 (決定 9) はダンプが引き継ぐ。本文を覗く口が 2 つあると、`max_body` の既定や loopback の制限のような作法を両方で保守することになる。tap の「その場で見る」用途は `dump start --follow` (決定 12) か `tail -f index.jsonl` で足りる。events (DR-0012) は恒常の読み手を持つ公開契約で、本文のような揮発的な観測はそこに混ぜない。

## 採らなかった案

reference に移った判断 (1 本 1 ファイル、系列を `dump_id` で独立させる、`persist` の 3 通り、申告値でなく実使用量で判定する、等) の理由は reference の各本にある。以下は llm-gateway で選ばなかった形。

- **設定ファイルの `[dump]` に on / off や対象 ns を書いて `daemon reload` で効かせる**: 消し忘れると常設になり、要件の「一時的に」と逆になる。`[dump]` に置くのは歯止めと既定値だけ (決定 3)
- **子の HTTP に `POST /llm-gateway/dump` を置く**: ダンプは本文をディスクへ書き残す操作で、外から届く面に置かない (決定 1)。DR-0032 が reload を HTTP に置かない構図と同じ
- **`daemon dump start|stop|list|prune`**: `daemon` はプロセス操作の語で、ダンプは unit の生死を触らない。`usage` / `stats` が最上位に居るのと同じ区分に置く (決定 1)
- **tap を残して併存させる**: 本文を覗く口が 2 つになり、`max_body` の既定や loopback の制限を両方で保守することになる。tap を `curl -N … > dump.jsonl` で取って済ませる案も、繋いでいるプロセスが生きている間しか取れず、置き場・権限・片付けは取得者の手に残る。購読者が止まった時に unit が本文を抱え続ける実測 (reference `model` の適用例) も tap 側の弱点 (決定 13)
- **動いている間の `start` は断るか、前の指定を置き換える (unit × ns で同時に 1 つ)**: 断ると延長のたびに `stop` → `start` が要り、その間の 1 本が抜ける。置き換えは「前の指定を引き継がない」規則と置き換えられた側の扱いを説明する必要が出る。系列を独立させれば延長は新しい `start`、短縮は `stop` で済む (reference `model`)
- **ns ごとの jsonl に追記する (1 ファイルの単位)**: response を生のまま残すには本文を JSON 文字列に包む変換が要り、調べたい 1 本だけを開けない (reference `files`)
- **ディレクトリ名を `start` の日時にする**: 同じ unit で同じ秒に 2 つ始めると衝突する。UUIDv7 は時刻順に並ぶ性質を保ったまま衝突しない (reference `model`)
- **定期の timer で片付ける**: 契機は unit の起動と `dump start` で足りる (決定 4)。間隔に根拠の無いループを足さない
- **全 ns を一括でダンプする旗**: 面の内容が別の面の作業で読まれる事故を減らす (決定 2)
- **ダンプ中の状態をディスクに書いて restart を跨いで続ける**: restart の後も溜まり続ける経路になる (reference `model`)

## 影響

- `GET /llm-gateway/tap`、`tap::` モジュール、`TapObservation` が消える (決定 13)。MANUAL と architecture-overview の tap の記述はダンプに置き換わる
- `Request` (監督者) と `ControlRequest` (unit の制御口) に dump の語が増える。`--follow` (決定 12) は 1 行の答えでなくストリームなので、監督者の中継は接続を保って行を通し、CLI 側の切断を unit まで伝える形が要る (DR-0032 の経路の拡張)
- 設定に `[dump]` テーブルが増える (決定 3)。`daemon reload` で差し替わると以後の `start` に効く
- `[stats] dir` の下に会話の本文が置かれる。keepalive の控えと同じ種類の機微で、置き場は 700 / 600 に閉じるが、ディスクを読める者には読める。ダンプしたファイルは会話の本文そのもので、リポや公開物に貼らない。外へコピーした先は取得者の責任 (reference `capacity`)
- 同じ ns に系列を重ねると、1 本が系列の数だけディスクに書かれる。重ねるのは利用者の選択で、`daemon status` に系列が並ぶので気づける
- `/llm-gateway/self` と `daemon status` の行に `dump` の欄が増える (決定 11)。欄を知らない読み手はそのまま動く
- `dump` の最上位コマンドが増え、help / MANUAL に載る (completion 定義はこのリポに無い)
- 認証差し替えの中継 (passthrough) は対象外 (決定 9)。中継の本文を残したくなったら別に決める
- 常時の観測は OTel (docs/issue/2026-10-21-otel-observability-for-dev-environment.md) の側で、ダンプは調査用の一時手段に留める。ダンプに常設の機能 (検索・集計) を足さない

### 実装の差し替えの段分け

系列と `dump_id`、制御 socket の経路、tap の除去、700 / 600、`dumps/` の片付け、`/self` と `daemon status` の `dump` は済んでいる。本版に合わせて差し替えるのは以下で、段ごとに完了条件を持つ。

| 段 | 中身 | 完了条件 |
|---|---|---|
| 1 | ファイル構成 (`*.request.json` + `*.request.body`、試行ごとの `*.response.json` + `*.response.body`、`body_file` で指す、`rewrites`、ヘッダの配列) と、index.jsonl を 1 本 1 行から時刻点ごとのイベント (reference `files` の表) に、1 秒以内の flush | 試験で、ns 認証を通したヘッダ受信の時点 (本文を読む前) で `request_headers` の行が 1 秒以内に書かれ、本文が来ないまま切れた 1 本にも `request_headers` が残り、読めなかった本文も理由付きで受けたまま残り、試行ごとに `upstream_headers` / `upstream_body_end` が並び、`response_headers_sent` の `adopted` が返した試行の番号、`stop` の行に `reason` と `bytes_written` が入る |
| 2 | `[dump]` テーブル (決定 3) と、全体の実使用量の上限・fs 空き容量の判定・`max_concurrency`、`start` の答えの `concurrency` / `storage`、片付けの基準を `delete_after` に | 試験で、`max_timeout` > `cleanup_ttl` の設定が `check` に落ち、全体上限に達した時に書こうとした系列だけが `total_limit` で止まり、request の来ない系列は `timeout` まで active のまま残る |
| 3 | `status` → `list` (選択子)、`prune`、`labels` | `stop` が冪等で元の `reason` を返し、`prune` が無指定を断り、active な系列を `stop` で止めてから消し、`list state=active` に動いている分だけが並ぶ |
| 4 | `--follow` (監督者のストリーム中継、`disconnected` / `lagged`)、`--embed bodies`、`--no-persist` | Ctrl-C で系列が `disconnected` で止まり、購読者を止めたままにすると `lagged` で切れ、`--no-persist` の系列が停止時にディレクトリごと消える |

## 未決

reference の未決のうち llm-gateway で確定したもの: `{ts}` と各イベントの `ts` は Unix ミリ秒 (決定 5)、メタ json のヘッダは `{name, value}` の配列 (決定 8)、follow の形式は ndjson (決定 12)、fs 空き容量の測り直しは 10 秒ごと、labels の長さと文字種 (決定 1)。

統括が 2026-10-21 に裁定したもの:

- `max_concurrency` の既定は 16、上限に達した `start` は `too_many_dumps` (決定 3・10)
- `rewrites` は試行ごとの `*.<attempt>.response.json` の `request_rewrites` に書き、`*.request.json` には置かない (決定 7)
- restart 前の系列は `list` に `state: "stopped"` で並び、`reason` は `stop` の行の値か `"interrupted"`、`prune` の対象 (決定 10)
- `daemon reload` で `[dump]` が変わったら以後の `start` に効き、動いている系列は始めた時の値のまま。指した ns が新しい設定から消えても系列は止めない (書く相手が無いまま時限まで残る)
- events (DR-0012) と stats (DR-0011) にはダンプ中を見せない (`daemon status` と `/self` で足りる)。全 unit に向ける旗は持たない (unit の数だけ `start` する)。`--request-only` は持たない (reference のとおり request と response の両方を残す)

実装で決めたもの:

- `--embed bodies` の行は、メタ json が `{"event":"file","file":<名前>,"content":<json>}`、本文が書いたチャンクごとの `{"event":"body","file":<名前>,"data_base64":<base64>}`。どちらも follow の接続にだけ流し、`index.jsonl` には書かない
- `--no-persist` の系列は、動いている間はファイルも書き (計数はファイルの側)、止まった時にディレクトリごと消す。reference の「送り終えた本文ファイルをその場で消し、全体の実使用量から引く」は持たない (動いている間の全体の実使用量は、送り終えた分も含めて数える)

## 関連

- claude-rules-personal の reference `proxy-dump-design` (`_index` / `model` / `capacity` / `files` / `api`) — 本 DR の正本。一般形はすべてここ
- docs/issue/2026-10-21-request-dump-with-timeout.md (本 DR の元。要件と補強案、裁定の正本)
- docs/research/2026-10-21-tap-body-capture-and-limits.md — tap の実測。決定 13 の根拠
- [DR-0032](DR-0032-daemon-reload.md) — CLI → 監督者 → unit の制御 socket。dump の経路と答えの形はこれに乗る (決定 1・10)
- [DR-0028](DR-0028-daemon-service-subcommands.md) — `daemon` の体系と、走っている unit に聞く系の `--unit` の規則 (決定 1)
- [DR-0011](DR-0011-daily-usage-stats.md) — relay の外で本文を覗く形と、writer を分けて排他を消す形 (決定 4)
- [DR-0012](DR-0012-request-events.md) — 時刻は Unix ミリ秒 (決定 5・10)。`seq` を揃える相手。events にはダンプ中を見せない
- [DR-0020](DR-0020-denial-reason-visibility.md) — 揮発的な観測は events に混ぜず、恒久の語彙は events に置く、の切り分け
- [DR-0024](DR-0024-cache-strategy-and-keepalive.md) — prompt cache の検証に response が要る。書き換えが経路ごとに効く (決定 7)
- [DR-0027](DR-0027-keepalive-by-replay.md) — `[stats] dir` に本文を置く先例と、秘密のヘッダの述語 (決定 8)
- [DR-0030](DR-0030-ns-auth-and-passthrough.md) — ns 認証の token と passthrough (決定 8・9)
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) §12 / §13 — `[stats] dir` の下に置く時の作法 (700 / 600、ns の名前の制限) と壁時計の扱い
- docs/issue/2026-10-21-otel-observability-for-dev-environment.md — 常時の観測は OTel 側、ダンプは一時手段
- claude-rules-personal の reference `cli-design/daemon-subcommands` — `daemon` の語の範囲 (決定 1)
