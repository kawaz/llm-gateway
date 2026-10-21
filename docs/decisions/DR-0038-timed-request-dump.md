# DR-0038: リクエストと応答の本文を、時限と容量の上限つきで一時的にダンプする `dump start|stop|status`

- Status: Accepted (kawaz が 2026-10-21 に着手を裁定。同日 DP-Q1 で tap の廃止・本文の形・系列 (dump_id) の形とファイルの単位を裁定。「未決」の節の項目は Proposed のまま)
- Date: 2026-10-21

## 背景

Claude Code 2.1.300 の jsonl 記録 (api-request / shape / blob) と gateway を実際に流れた本文を突き合わせようとして、gateway 側に本文を保存する手段が無いことに当たった (2026-10-21)。DR-0024 の prompt cache の検証や「upstream が何を返したか」の調査でも、毎回同じ所で困る。

本文を覗く口は 2 つあった。tap (`GET /llm-gateway/tap`) は `curl -N` で繋いでいる間だけ流れる揮発的な口で、繋いでいるプロセスが要り、本文は既定 64 KiB で切り詰められ、応答は終端まで溜めてから 1 件で配る。keepalive の控え (DR-0027) は送り直すための本文で、cache に乗った 1 本だけを `[stats] dir` の下に置き、調査のために残すものではない。どちらも「数分だけ全量を取って、後で突き合わせる」には向かない (tap の実測は `docs/research/2026-10-21-tap-body-capture-and-limits.md`: `max_body` に上限は無く 9.7MB の 1 本も全量取れる、既定の 64 KiB は稼働中の request 64/64 本が超える、応答は中央値 18KB で既定に収まる)。

kawaz の要件 (2026-10-21) は 3 つ。常時ではなく一時的に有効化できること、溜まり事故を防ぐため外部から on/off でき必ず時限で切れること、片付けを忘れるので 48 時間より古いダンプは勝手に消えること。本 DR は要件の形 (CLI、経路、置き場、片付け、ファイルの形) を決め、容量の既定値など実装に委ねてよい点は未決として残す。

## 決定

各項の末尾の印は出所。(kawaz 要件) は 2026-10-21 の要件そのもの、(kawaz 裁定) は DP-Q1 とその後の裁定、(統括の補強案) は issue に書かれた補強案、(起草者の判断) は本 DR で決めた点で、kawaz が覆してよい。

### 1. 有効化は必ず時限で、timer は unit が持ち restart で消える (kawaz 要件)

```
llm-gateway dump start --ns <ns> --for <duration> [--unit <unit>] [--max-bytes <n>] [--request-only]
llm-gateway dump stop  <dump_id>|--all [--unit <unit>]
llm-gateway dump status [--unit <unit>]
```

- `--for` は必須で、既定値を持たない。時限は要件の芯で、既定値を持つと「書かなかったら何分か」を覚える必要が出る。書き方は設定の duration (`10m` / `1h`) と同じ
- 終わりの時刻 (`until`) は `start` を受けた壁時計に `--for` を足して決め、unit がメモリに持つ。`daemon restart` や落ちた後の起こし直しで消え、上がった unit はダンプしていない状態で始まる。ディスクに「ダンプ中」を書かない (書くと restart を跨いで溜まり続ける経路ができる)
- 時限は timer の発火だけに頼らず、書く時点で `until` と壁時計を比べて守る。macOS のスリープで単調時計が止まり timer が遅れても (DR-0037 §13)、`until` を過ぎた 1 本は書かない (起草者の判断)

### 2. on/off の経路は制御 socket で、設定ファイルには書かない (統括の補強案)

DR-0032 決定 2 の経路をそのまま使う。CLI → 監督者 (`supervisor.sock`、`Request` に dump の語を足す) → unit の制御口 (`daemon/control/<unit>.sock`、`ControlRequest` に足す) で、unit は結果を 1 行の JSON で返す。答えの形は DR-0032 決定 5 の `{unit, ok, error}` の流儀に揃え、`ok: true` の答えに決定 11 の欄を同じ階層で添える (`{"unit", "ok": true, "dump_id", …}` / `{"unit", "ok": true, "dumps": […]}`)。断る時は `ok: false` と `error: {kind, message}` で、`kind` は `unit_required` / `unknown_namespace` / `unknown_dump` / `supervisor_not_running` 等 (起草者の判断)。

- 設定ファイルに `[dump]` のような欄を持たない。設定に書いたものは消し忘れでそのまま常設になり、要件の「一時的に」と逆になる。`daemon reload` の対象にもならない
- 子の HTTP には置かない。ダンプは会話の本文をディスクへ書く操作で、外 (Caddy 越し) から届く面に置かない理由は DR-0032 が reload を HTTP に置かない理由と同じ
- 監督者が居なければ `start` / `stop` / `status` は `supervisor_not_running` で断る (DR-0028 決定 3)。制御口を持たない版の unit には `ok: false` を返す (DR-0032 の「版の混在」と同じ扱い)

### 3. CLI は最上位の `dump`。`daemon dump` にしない (起草者の判断)

`daemon` は「この instance のプロセス操作」(DR-0028 決定 1、`cli-daemon-subcommands`) で、`reload` は設定を読み直す操作だからそこに居る。ダンプは unit を上げ下げも読み直しもせず、流れている交換を覗く操作で、`usage` / `stats` / `upstream status` と同じ「走っている unit に聞く・頼む」系に属する。だから最上位に置き、unit の指し方は DR-0028 決定 6 に従う。

- `--unit <unit>` で指す。登録が 1 つならそれを使い、複数あって未指定なら `unit_required` で名前を求める (`usage` / `stats` と同じ `destination::resolve` の規則)。経路が HTTP でなく監督者の socket である点だけが違い、利用者の指し方は変わらない
- `dump` は子を持つレベルなので、引数なしと `--help` はテキストの help を出す。`start` は必須の引数 (`--ns` / `--for`) を持つので、引数なしなら help。`stop` は `<dump_id>` か `--all` のどちらかが要り、どちらも無ければ help。`status` は省略可能な引数だけなので実行する
- `start` のたびに新しい系列 (決定 13) が始まる。`stop <dump_id>` はその系列を時限を待たず止め、`stop --all` はその unit で動いている全系列を止める。`stop` は冪等で、既に止まっている系列には `active: false` で成功する (DR-0028 決定 7 の `register` と同じ姿勢)。unit が知らない id は `unknown_dump` で断る (restart で消えた系列の id もここに入る)
- `--help` の文面・実装・zsh completion の 3 者を揃える (cli-design-preferences)

### 4. `--ns` は必須で、全 ns 一括はしない (統括の補強案)

1 回の `start` は 1 つの ns だけを対象にする。ns を書かない・全部を指す形は持たない。面の内容が別の面の作業で読まれる事故を減らすためで、同じ理由で置き場も ns ごとに分ける (決定 6)。指した ns が走っている設定に無ければ `unknown_namespace` で断る。複数の ns を取りたければ ns ごとに `start` し、それぞれが独立した系列になる (決定 13)。

### 5. 容量の上限も持ち、超えたら止まる (統括の補強案)

`--max-bytes <n>` で、書いたバイト数がそれを超えた時点で時限を待たずに止まる。上限を越えて書き足さない。並列の worker から 1 本数百 KB の本文が重なると、10 分でも GB に届きうる。

- 数えるのは系列ごとで、その系列のディレクトリ (決定 6) に書いた分だけ。同じ ns の別の系列や古いディレクトリは含めない。古いものは決定 9 の片付けが時間で面倒を見る (起草者の判断)
- request は書く前に大きさが分かるので、書くと上限を超えるなら書かずに止まる。response は通り過ぎるチャンクをその都度書く (決定 10) ので大きさを先に知れず、上限に達したところで打ち切って止まる。打ち切った 1 本は `index.jsonl` (決定 14) の行に `truncated: true` が付く (起草者の判断)
- 既定値は未決 1。issue の案は 200 MB 程度

### 6. 置き場は `[stats] dir` の `dumps/<unit>/<ns>/<dump_id>/`、ディレクトリ 700、ファイル 600 (統括の補強案 + kawaz 裁定)

`[stats] dir` は keepalive の控え (DR-0027)、thinking の出所 (DR-0035 §6)、自動の鍵 (DR-0037 §12) が使う「兄弟の unit が共有する走行状態」の置き場で、ダンプも同じ性質を持つ。新しい置き場のキーは足さない。

- `<unit>` を挟むので書き手が unit ごとに分かれ、`.lock` の flock は要らない (DR-0011 の「writer 毎に分けて排他を消す」と同じ)。stable と unstable が同じ ns を同時にダンプしても互いのファイルに触らない
- `<ns>` を挟むのは決定 4 の理由。ns の名前はディレクトリ名になるので、DR-0037 §12 と同じ制限 (英数字と `.` `_` `-`、`.` で始まらない) を `start` の時に確かめ、通らない名前は断る
- `<dump_id>` は系列 (決定 13) ごとのディレクトリで、`start` 1 回 = 1 ディレクトリ。系列の中身 (決定 14) はここに閉じ、別の系列のディレクトリには触らない
- ディレクトリは 700、ファイルは 600。会話の本文が入るので、自動の鍵のファイルと同じ閉じ方にする
- `start` と `dump status` の答えに系列のディレクトリの絶対パス (`dump_dir`) を載せる。突き合わせる人が置き場を導出し直さずに済む

### 7. request は gateway が受けた形、response は upstream が返した生のバイト列。`--request-only` で絞れる (kawaz 裁定 + 統括の補強案)

cache の検証と upstream の応答の調査には response が要る。ダンプに残すのは両端の生の姿だけで、gateway の手が入った中間の形は残さない。

- request は gateway が受けた本文 (ハーネスが送ろうとしたもの)。Claude Code の api-request 記録と同じものを表し、`requestDigest` まで突き合わせられる (research の A3)。gateway の書き換え (cache の `ttl` の付け替え、thinking の display の上書き、model の別名の解決、thinking_as_text の変換) は載らない。書き換えの有無は events (DR-0012: `cache_ttl_secs` / `thinking_as_text` 等) と stats (DR-0011: 5m / 1h の内訳) と応答の usage から読む
- response は解釈せず、upstream が返したバイト列 (SSE なら `event:` / `data:` の行そのまま) を残す。upstream が加工後の request に答えた結果なので、書き換えの効き方はここから読める
- `--request-only` は request だけを書き、response を書かない。response だけを取る旗は持たない (要件が無い)
- 1 本に添えるメタデータは `index.jsonl` の 1 行 (決定 14)。時刻は Unix ミリ秒 (DR-0012 の規約) で、欄の定義は events の `request` と揃え、2 つ持たない (起草者の判断)

### 8. 秘密のヘッダは落とす。述語は keepalive の控えと同じ (統括の補強案)

issue の案は Authorization を落とすことだが、gateway は控えに持ち回してはいけないヘッダの述語を既に 1 つ持っている (`gateway.rs` の `is_authorization`: `authorization` / `proxy-authorization` / `x-api-key` / `cookie`、DR-0027 決定 1)。ダンプも同じ述語で落とし、2 つ目の一覧を持たない (起草者の判断)。受けた形の `Authorization` には ns 認証の token (DR-0030) が入るので、落とす対象はそこ。

秘密以外のヘッダをどこまで残すかは未決 2。

### 9. 片付けは unit の起動時と `dump start` の時に、48 時間より古い系列をディレクトリごと消す (kawaz 要件 + 統括の補強案 + kawaz 裁定)

定期の timer を足さない。契機は unit が上がった時と `dump start` を受けた時の 2 つで、どちらも「ダンプに関心が向いた瞬間」だから、そこで掃けば溜まらない。

- 消す単位は系列のディレクトリ (`<dump_id>/`) で、中のファイルを個別には消さない。1 つの系列は丸ごと残るか丸ごと消えるかのどちらかで、`index.jsonl` だけ残って本文が無い、のような半端が出ない
- 古さは系列のディレクトリの mtime で判定し、48 時間より古いものを消す (起草者の判断)。ディレクトリの mtime は中にファイルが作られるたびに進むので、「最後に 1 本書いてから 48 時間」になる。書いている最中の系列は常に新しく、兄弟 unit が動かしている系列を消すことがない。`dump_id` の時刻部 (決定 13) で判定すると、`--for` が 48 時間を超えた系列を書いている最中に消しうるので使わない。空になった `<ns>/` と `<unit>/` も消す
- 掃く範囲は `dumps/` の下全体で、他の unit の分も含む (起草者の判断)。止まったままの unit は自分の分を掃けないので、どれかの unit が上がれば全部が片付く形にする
- `--for` の上限は未決 4

### 10. 控える位置は受け口と `exchange::observe` の包みで、relay は無変更 (起草者の判断)

request は受け口 (`llm-gateway-server` の `messages` / `responses`) で本文を読み切った直後、書き換える前に控える。response は relay の外の `exchange::observe` の包み (`BodyObservation`) で、通り過ぎるチャンクをそのまま書く。relay は無変更 (DR-0011 の「本文を覗く層は relay の外に置く」)。

- response は終端で 1 件にまとめず、通り過ぎるチャンクをそのまま書く。終端まで溜めると、上限いっぱいの本文が並列の数だけメモリに乗る
- 認証差し替えの中継 (DR-0030 §2 の passthrough) は本文を読まずに流す口で、ダンプの対象にしない
- 動いている系列が無い間は 1 バイトも控えない。「誰も求めていなければ何もしない」を atomic の load 1 つで判定し、中継の速度を落とさない。1 本をどの系列に書くかは request を受けた時点で決まり、途中から始めた系列にその 1 本は入らない

### 11. `start` / `stop` / `status` の答え (kawaz 裁定 + 起草者の判断)

```
llm-gateway dump start --ns lab --for 10m --unit unstable
{"unit":"unstable","ok":true,"dump_id":"019a1b2c-3d4e-7f80-9123-456789abcdef",
 "dump_dir":"~/.local/state/llm-gateway/stats/dumps/unstable/lab/019a1b2c-3d4e-7f80-9123-456789abcdef",
 "ns":"lab","until":1792000600000,"max_bytes":209715200,"request_only":false}

llm-gateway dump status --unit unstable
{"unit":"unstable","ok":true,"dumps":[
 {"dump_id":"019a1b2c-3d4e-7f80-9123-456789abcdef","dump_dir":"…/dumps/unstable/lab/019a1b2c-…",
  "ns":"lab","until":1792000600000,"max_bytes":209715200,"request_only":false,
  "bytes_written":1234567,"requests":12}]}
```

- 答えはどれも `unit` と `ok: true` で始まる (決定 2)。`start` は始めた系列の `dump_id` と `dump_dir` に、受け取った引数の全部 (`ns` / `until` / `max_bytes` / `request_only`) を添えて返す。`until` は決めた終わりの時刻で、`--for` そのものは返さない
- `status` は**走っている系列**の一覧で、`start` の欄に `bytes_written` (その系列に書いた合計) と `requests` (書いた本数) を足した 1 件ずつを `dumps` に並べる。止まった系列は載らず、1 つも動いていなければ空の配列
- `stop <dump_id>` は止めた系列の 1 件 (`status` の欄に `active: false` を足した形) を返す。既に止まっていた系列も同じ形で `active: false`。`stop --all` は止めた系列の 1 件ずつを `dumps` に並べる (動いていなければ空の配列)
- 系列が止まった理由 (`expired` / `max_bytes` / `stopped`) は unit の log に出す。答えの欄には持たない (起草者の判断)
- 時刻は DR-0012 の規約 (単位を名前に持たない数値 = Unix ミリ秒)。`until` を出すので残り時間の引き算は読む側がする。`dump_dir` は決定 6 の絶対パス (例は `~` で書いたが、実際には展開した値)

### 12. `daemon status` の各行にも、動いている間だけ出す (起草者の判断)

溜まり事故を防ぐのが要件の芯で、普段見る `daemon status` に出ていれば止め忘れに気づける。unit は `/llm-gateway/self` に `dump: [{dump_id, ns, until, bytes_written}]` (動いている系列の分だけ) を載せ、監督者は DR-0028 決定 5 の経路 (`/self` の 1 回の問い合わせ) で各行に写す。1 つも動いていなければ欄ごと出さない。詳しい値は `dump status` が答える。

`/self` は認証の無い口だが、載せるのは id と ns の名前と時刻と大きさまでで、events (DR-0012) が既に流している以上の情報は含まない。本文の置き場も載せない (それは制御 socket 越しの `dump status` だけが答える)。

### 13. `start` 1 回 = 1 系列。系列は `dump_id` (UUIDv7) で指し、同じ ns に期間が重なる系列を複数作れる (kawaz 裁定)

`start` のたびに unit が `dump_id` を UUIDv7 で作り、系列 (時限・上限・`--request-only`・書いた量・ディレクトリ) はすべて `dump_id` に付く。系列どうしは互いを知らず、同じ ns に期間の重なる系列が何本あってもよい。重なっている間の 1 本の request は、動いている系列の全部に書かれる (ns ごとの計数にも、それぞれの系列で同じ大きさが乗る)。これは明示的に許す結果で、重なりを断ったり前の系列を置き換えたりしない。

- 系列の数に上限を置かない。`--for` が必須なので放っておいても減る
- `dump_id` が UUIDv7 なのは、時刻を先頭に持つので `ls` で系列が始まった順に並び、別の unit が同じ ns に同時に始めても衝突しないため。ディレクトリ名 (決定 6) と `stop` の引数と `status` / `/self` の欄はすべてこの id
- `start` の置き換えや `unit × ns で 1 つ` の制限は持たない。「延長したい」は新しい系列を始めることで足り、前の系列は自分の `until` で止まる。「短くしたい」は `stop <dump_id>`

### 14. 系列のディレクトリの中身: `index.jsonl` と、1 本ごとの `<ts>-<seq>.request.json` / `.response.sse` (kawaz 裁定)

```
dumps/unstable/lab/019a1b2c-3d4e-7f80-9123-456789abcdef/
  index.jsonl
  1792000012345-42.request.json
  1792000012345-42.response.sse
  1792000013210-43.request.json
  1792000013210-43.response.sse
```

- `index.jsonl` は 1 本 1 行のメタデータで、系列の入口。欄は `ts` (request を受けた時刻、Unix ミリ秒) / `seq` (unit の通し番号、`exchange::request_span` のもの) / `model` / `route` / `status` / `origin` / `stream` / `credential` / `cache_strategy` / `cache_ttl_secs` / `request_bytes` / `response_bytes` / `request_file` / `response_file` と、打ち切った 1 本の `truncated: true` (決定 5)。欄の意味は events の `request` と同じ (決定 7)。行は応答の終端で書くので、並びは終わった順
- `<ts>-<seq>.request.json` は受けた本文そのまま (決定 7)。`<ts>` と `<seq>` は `index.jsonl` の同じ欄で、ファイル名だけで時刻順に並び、同じミリ秒の 2 本も `seq` で分かれる
- response のファイルは常に `.response.sse` で、中身は upstream が返した生のバイト列 (SSE なら `event:` / `data:` の行、SSE でない応答ならその JSON 1 本)。応答の種類で拡張子を分けない (分けると扱いにくい、kawaz 2026-10-21)。SSE かどうかは `index.jsonl` の `stream` と応答の `content-type` で分かる。`--request-only` の系列では response のファイルも `response_file` の欄も無い
- 1 本 1 ファイルにするのは、response を生のまま残すのに 1 ファイル追記 (jsonl) だと本文を JSON 文字列に包む変換が要り、伸び続けるファイルは mtime が常に新しくて片付けが部分的にしか効かず、調べたい 1 本だけを開けないため。系列あたり数百〜千のファイルになる懸念は、`index.jsonl` を入口にして `request_file` / `response_file` から辿ることで受ける
- ヘッダをどこに置くかは未決 2 (本文のファイルに含めるか、`index.jsonl` の行に載せるか)

### 15. tap を廃止し、本文を控える口はダンプ 1 つにする (kawaz 裁定)

tap (DR-0017) を廃止する: `GET /llm-gateway/tap` と `tap::` モジュール、`TapObservation` を取り除き、本文を控える位置 (決定 10) はダンプが引き継ぐ。本文を覗く口が 2 つあると、`max_body` の既定や loopback の制限のような作法を両方で保守することになる。tap の「その場で見る」用途は、系列のディレクトリを `ls` し `index.jsonl` を `tail -f` すれば足りる。

events (DR-0012) は恒常の読み手 (ccmsg 等) を持つ公開契約で、本文のような揮発的な観測はそこに混ぜない。本文を見たければダンプを始める。

## 採らなかった案

- **設定ファイルに `[dump]` を書いて `daemon reload` で効かせる**: 消し忘れると常設になり、要件の「一時的に」と逆になる。書き換えを reload する手順も、時限で切れる形にできない (決定 2)
- **子の HTTP に `POST /llm-gateway/dump` を置く**: ダンプは本文をディスクへ書き残す操作で、外から届く面に置かない (決定 2)。DR-0032 が reload を HTTP に置かない構図と同じ
- **`daemon dump start|stop|status`**: `daemon` はプロセス操作の語で、ダンプは unit の生死を触らない。`usage` / `stats` が最上位に居るのと同じ区分に置く (決定 3)
- **tap を残して併存させる**: 本文を覗く口が 2 つになり、`max_body` の既定や loopback の制限を両方で保守することになる。tap で「その場で見る」のは、系列のディレクトリの `tail -f` で足りる。tap を `curl -N … > dump.jsonl` で取って済ませる案も、繋いでいるプロセスが生きている間しか取れず、`timeout` で時限にしても置き場・権限・片付けは取得者の手に残る。本文は既定 64 KiB で切り詰められ、`max_body` を明示する必要がある。メモリは応答の溜め込みより、購読者が止まった時に unit が抱える最大 64 件 × 64 MiB の request の控えが効く (実測: 購読者を SIGSTOP で止めると 9.7MB × 4 本で +39MB を持ち続け、再開時に `lagged` で切断) (決定 15)
- **upstream へ送った形を残す (両方を書く)**: 書き換えの有無は events と stats と応答の usage から読め、本文を 2 通り残すと置き場も計数も倍になる。突き合わせの相手 (Claude Code の記録) は受けた形なので、残すのは両端の生の姿だけ (決定 7)
- **1 つの unit (または unit × ns) で同時に 1 つだけにし、動いている間の `start` は断るか指定を置き換える**: 断ると延長のたびに `stop` → `start` が要り、その間の 1 本が抜ける。置き換えは「前の指定を引き継がない」規則と `replaced` の返却を足し、置き換えられた側の扱い (ファイルは残るが計数は消える) を説明する必要が出る。系列を `dump_id` で独立させれば、延長は新しい `start`、短縮は `stop` で済み、規則が 1 つも要らない (決定 13)
- **ns ごとの jsonl に追記する (1 ファイルの単位)**: response を生のまま残すには本文を JSON 文字列に包む変換が要る。伸びるファイルは mtime が常に新しく、48 時間の片付けが部分的にしか効かない。調べたい 1 本だけを開けない (決定 14)
- **ディレクトリ名を `start` の日時にする**: 同じ ns に同じ秒で 2 つの系列が始まると衝突する (兄弟 unit は `<unit>/` で分かれるが、同じ unit での連打は分かれない)。UUIDv7 は時刻順に並ぶ性質を保ったまま衝突しない (決定 13)
- **片付けの古さを `dump_id` の時刻部 (= `start` の時刻) で判定する**: `--for` が 48 時間を超えた系列を、書いている最中に消しうる。ディレクトリの mtime なら書いている最中は常に新しい (決定 9)
- **定期の timer で 48 時間超を掃く**: 契機は unit の起動と `dump start` で足りる (決定 9)。間隔に根拠の無いループを足さない
- **全 ns を一括でダンプする旗**: 面の内容が別の面の作業で読まれる事故を減らす (決定 4)
- **ダンプ中の状態をディスクに書いて restart を跨いで続ける**: restart の後も溜まり続ける経路になる。timer は unit が持ち、restart で消える (決定 1)

## 影響

- `GET /llm-gateway/tap`、`tap::` モジュール、`TapObservation` が消える (決定 15)。MANUAL の tap の節と architecture-overview の tap の記述はダンプに置き換わる
- `Request` (監督者) と `ControlRequest` (unit の制御口) に dump の語が増える。監督者と unit は別の binary でありうる (DR-0028 決定 2) ので、制御口が dump を知らない版の unit には `ok: false` を返す (DR-0032 の「版の混在」と同じ)
- `[stats] dir` の下に会話の本文が置かれる。keepalive の控えと同じ種類の機微で、置き場は 700 / 600 に閉じるが、ディスクを読める者には読める。ダンプしたファイルは会話の本文そのもので、リポや公開物に貼らない。置き場の中は 48 時間の片付けが面倒を見るが、外へコピーした先は取得者の責任
- 同じ ns に系列を重ねると、1 本が系列の数だけディスクに書かれる (決定 13)。重ねるのは利用者の選択で、`daemon status` に系列が並ぶので気づける
- `/llm-gateway/self` と `daemon status` の行に `dump` の欄が増える (決定 12)。欄を知らない読み手はそのまま動く
- `dump` の最上位コマンドが増え、help / MANUAL / zsh completion に載る
- 認証差し替えの中継 (passthrough) は対象外 (決定 10)。中継の本文を残したくなったら別に決める
- 常時の観測は OTel (docs/issue/2026-10-21-otel-observability-for-dev-environment.md) の側で、ダンプは調査用の一時手段に留める。ダンプに常設の機能 (検索・集計) を足さない

### 実装の段分け案

| 段 | 中身 | 完了条件 |
|---|---|---|
| 1 | unit 側: tap の取り外しと、同じ位置でのダンプの書き出し (系列の表: dump_id / ns / until / 上限 / 書いた量、`index.jsonl` と 1 本ごとのファイル)、起動時と start 時の片付け、`/self` への露出 | 試験で、`until` を過ぎた 1 本と上限を超える 1 本が書かれず、重なる 2 系列に同じ 1 本が両方書かれ、48 時間より古い系列だけがディレクトリごと消え、`GET /llm-gateway/tap` が無い |
| 2 | 制御口と監督者の中継 (`ControlRequest` / `Request`)、答えの形 (決定 2・11) | 試験で、制御口を持たない unit に `ok: false` が返り、知らない id の `stop` が `ok: false` + `unknown_dump` になる |
| 3 | CLI `dump start|stop|status`、`--help`・zsh completion・MANUAL | `dump start --ns lab --for 10m` が `dump_id` と `dump_dir` を答え、`daemon status` の行に `dump` が出て、10 分後に消える |

## 未決

Proposed のまま残す。実装の着手を止めるものではなく、着手した worker が判断して DR に書き足すか、kawaz が裁定する。

1. **容量上限の既定値**: issue の案は 200 MB 程度。並列 worker の本数と 1 本の大きさ (数百 KB) から決める
2. **秘密以外のヘッダをどこまで残し、どこに置くか**: `anthropic-beta` や `x-claude-code-*` のように突き合わせに要るものと、`user-agent` のように要らないもの。全部残して決定 8 の述語で秘密だけ落とすのが最も単純。置き場は `index.jsonl` の行 (本文のファイルを生のまま保てる) か、本文のファイルに包むか
3. **events (DR-0012) と stats (DR-0011) にダンプ中を見せるか**: events に `dump` の種類を足すと恒常の読み手に揺れが及ぶ (決定 15 の理由)。`daemon status` と `/self` (決定 12) で足りるなら足さない
4. **`--for` の上限**: 48 時間の片付け (決定 9) は mtime 判定なので動いている系列を消すことはないが、48 時間を超える時限は「一時的に」の要件から外れる。上限を置くか、置くなら値
5. **全 unit に向ける旗**: Caddy は 11301 を優先し落ちていれば 11302 に回すので、調査中に両方へ流れうる。`stop --all` は「その unit の全系列」を指す (決定 3) ので、全 unit を指す旗は別の綴りが要る。unit の数だけ `start` を繰り返せば同じ結果になるので、旗を持つか自体も含めて未決
6. **`daemon reload` で設定が差し替わった時**: 指した ns が新しい設定から消えたらその場で系列を止めるか、`until` まで (書く相手が無いまま) 続けるか

## 関連

- docs/issue/2026-10-21-request-dump-with-timeout.md (本 DR の元。要件と補強案、裁定の正本)
- docs/research/2026-10-21-tap-body-capture-and-limits.md — tap の実測 (受けた形であること、`max_body`・メモリ・切断の限界)。決定 7・15 の根拠
- [DR-0032](DR-0032-daemon-reload.md) — CLI → 監督者 → unit の制御 socket。dump の on/off はこの経路に乗る
- [DR-0028](DR-0028-daemon-service-subcommands.md) — `daemon` の体系と、走っている unit に聞く系の `--unit` の規則 (決定 6)
- [DR-0011](DR-0011-daily-usage-stats.md) — relay の外で本文を覗く形と、writer を分けて排他を消す形
- [DR-0012](DR-0012-request-events.md) — 時刻は Unix ミリ秒。`index.jsonl` の欄を揃える相手。events に足すかは未決 3
- [DR-0020](DR-0020-denial-reason-visibility.md) — 揮発的な観測は events に混ぜず、恒久の語彙は events に置く、の切り分け
- [DR-0027](DR-0027-keepalive-by-replay.md) — `[stats] dir` に本文を置く先例と、秘密のヘッダの述語
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) §12 / §13 — `[stats] dir` の下に置く時の作法 (700 / 600、ns の名前の制限) と壁時計の扱い
- [DR-0024](DR-0024-cache-strategy-and-keepalive.md) — prompt cache の検証に response が要る
- docs/issue/2026-10-21-otel-observability-for-dev-environment.md — 常時の観測は OTel 側、ダンプは一時手段
- claude-rules-personal の reference `cli-design/daemon-subcommands` — `daemon` の語の範囲
