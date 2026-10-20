# DR-0038: リクエストと応答の本文を、時限と容量の上限つきで一時的にダンプする `dump start|stop|status`

- Status: Accepted (kawaz が 2026-10-21 に着手を裁定。「未決」の節の項目は Proposed のまま)
- Date: 2026-10-21

## 背景

Claude Code 2.1.300 の jsonl 記録 (api-request / shape / blob) と gateway を実際に流れた本文を突き合わせようとして、gateway 側に本文を保存する手段が無いことに当たった (2026-10-21)。DR-0024 の prompt cache の検証や「upstream が何を返したか」の調査でも、毎回同じ所で困る。

本文を覗く口は既に 2 つある。tap (DR-0017) は `curl -N` で繋いでいる間だけ流れる揮発的な口で、繋いでいるプロセスが要り、本文は既定 64 KiB で切り詰められ、応答は終端まで溜めてから 1 件で配る。keepalive の控え (DR-0027) は送り直すための本文で、cache に乗った 1 本だけを `[stats] dir` の下に置き、調査のために残すものではない。どちらも「数分だけ全量を取って、後で突き合わせる」には向かない (tap の実測は `docs/research/2026-10-21-tap-body-capture-and-limits.md`: `max_body` に上限は無く 9.7MB の 1 本も全量取れる、既定の 64 KiB は稼働中の request 64/64 本が超える、応答は中央値 18KB で既定に収まる)。

kawaz の要件 (2026-10-21) は 3 つ。常時ではなく一時的に有効化できること、溜まり事故を防ぐため外部から on/off でき必ず時限で切れること、片付けを忘れるので 48 時間より古いダンプは勝手に消えること。本 DR は要件の形 (CLI、経路、置き場、片付け) を決め、1 ファイルの単位など実装に委ねてよい点は未決として残す。

## 決定

各項の末尾の印は出所。(kawaz 要件) は 2026-10-21 の要件そのもの、(統括の補強案) は issue に書かれた補強案、(起草者の判断) は本 DR で決めた点で、kawaz が覆してよい。

### 1. 有効化は必ず時限で、timer は unit が持ち restart で消える (kawaz 要件)

```
llm-gateway dump start --ns <ns> --for <duration> [--unit <unit>] [--max-bytes <n>] [--request-only]
llm-gateway dump stop  [--unit <unit>]
llm-gateway dump status [--unit <unit>]
```

- `--for` は必須で、既定値を持たない。時限は要件の芯で、既定値を持つと「書かなかったら何分か」を覚える必要が出る。書き方は設定の duration (`10m` / `1h`) と同じ
- 終わりの時刻 (`until`) は `start` を受けた壁時計に `--for` を足して決め、unit がメモリに持つ。`daemon restart` や落ちた後の起こし直しで消え、上がった unit はダンプしていない状態で始まる。ディスクに「ダンプ中」を書かない (書くと restart を跨いで溜まり続ける経路ができる)
- 時限は timer の発火だけに頼らず、書く時点で `until` と壁時計を比べて守る。macOS のスリープで単調時計が止まり timer が遅れても (DR-0037 §13)、`until` を過ぎた 1 本は書かない (起草者の判断)

### 2. on/off の経路は制御 socket で、設定ファイルには書かない (統括の補強案)

DR-0032 決定 2 の経路をそのまま使う。CLI → 監督者 (`supervisor.sock`、`Request` に dump の語を足す) → unit の制御口 (`daemon/control/<unit>.sock`、`ControlRequest` に足す) で、unit は結果を 1 行の JSON で返す。答えの形は DR-0032 決定 5 の `{unit, ok, error}` の流儀に揃え、`ok: true` の答えには決定 11 の status と同じ中身を添える。

- 設定ファイルに `[dump]` のような欄を持たない。設定に書いたものは消し忘れでそのまま常設になり、要件の「一時的に」と逆になる。`daemon reload` の対象にもならない
- 子の HTTP には置かない。ダンプは会話の本文をディスクへ書く操作で、外 (Caddy 越し) から届く面に置かない理由は DR-0032 が reload を HTTP に置かない理由と同じ
- 監督者が居なければ `start` / `stop` / `status` は `supervisor_not_running` で断る (DR-0028 決定 3)。制御口を持たない版の unit には `ok: false` を返す (DR-0032 の「版の混在」と同じ扱い)

### 3. CLI は最上位の `dump`。`daemon dump` にしない (起草者の判断)

`daemon` は「この instance のプロセス操作」(DR-0028 決定 1、`cli-daemon-subcommands`) で、`reload` は設定を読み直す操作だからそこに居る。ダンプは unit を上げ下げも読み直しもせず、流れている交換を覗く操作で、`usage` / `stats` / `upstream status` と同じ「走っている unit に聞く・頼む」系に属する。だから最上位に置き、unit の指し方は DR-0028 決定 6 に従う。

- `--unit <unit>` で指す。登録が 1 つならそれを使い、複数あって未指定なら `unit_required` で名前を求める (`usage` / `stats` と同じ `destination::resolve` の規則)。経路が HTTP でなく監督者の socket である点だけが違い、利用者の指し方は変わらない
- `dump` は子を持つレベルなので、引数なしと `--help` はテキストの help を出す。`start` は必須の引数 (`--ns` / `--for`) を持つので、引数なしなら help。`stop` / `status` は省略可能な引数だけなので実行する
- 1 つの unit で同時に動くダンプは 1 つ。動いている間の `start` は `dump_active` で断る (2 つ目を受けると時限と容量をどちらで数えるかが曖昧になる)。`stop` は冪等で、動いていなければ `active: false` を返して成功する (DR-0028 決定 7 の `register` と同じ姿勢)。`--all` は持たない (未決 7)
- `--help` の文面・実装・zsh completion の 3 者を揃える (cli-design-preferences)

### 4. `--ns` は必須で、全 ns 一括はしない (統括の補強案)

1 回の `start` は 1 つの ns だけを対象にする。ns を書かない・全部を指す形は持たない。面の内容が別の面の作業で読まれる事故を減らすためで、同じ理由で置き場も ns ごとに分ける (決定 6)。指した ns が走っている設定に無ければ `unknown_namespace` で断る。

### 5. 容量の上限も持ち、超えたら止まる (統括の補強案)

`--max-bytes <n>` で、書いたバイト数がそれを超えた時点で時限を待たずに止まる。上限を越えて書き足さない。並列の worker から 1 本数百 KB の本文が重なると、10 分でも GB に届きうる。

- 数えるのは今回の `start` から書いた分で、同じ ns の古いファイルは含めない。古いファイルは決定 9 の片付けが時間で面倒を見る (起草者の判断)
- 既定値は未決 1。issue の案は 200 MB 程度

### 6. 置き場は `[stats] dir` の `dumps/<unit>/<ns>/`、ディレクトリ 700、ファイル 600 (統括の補強案)

`[stats] dir` は keepalive の控え (DR-0027)、thinking の出所 (DR-0035 §6)、自動の鍵 (DR-0037 §12) が使う「兄弟の unit が共有する走行状態」の置き場で、ダンプも同じ性質を持つ。新しい置き場のキーは足さない。

- `<unit>` を挟むので書き手が unit ごとに分かれ、`.lock` の flock は要らない (DR-0011 の「writer 毎に分けて排他を消す」と同じ)。stable と unstable が同じ ns を同時にダンプしても互いのファイルに触らない
- `<ns>` を挟むのは決定 4 の理由。ns の名前はディレクトリ名になるので、DR-0037 §12 と同じ制限 (英数字と `.` `_` `-`、`.` で始まらない) を `start` の時に確かめ、通らない名前は断る
- ディレクトリは 700、ファイルは 600。会話の本文が入るので、自動の鍵のファイルと同じ閉じ方にする
- `dump status` と `start` の答えに置き場の絶対パスを載せる。突き合わせる人が置き場を導出し直さずに済む

### 7. request と response の両方を取り、SSE は生のまま。`--request-only` で絞れる (統括の補強案)

cache の検証と upstream の応答の調査には response が要る。response は解釈せず、upstream が返したバイト列 (SSE なら `event:` / `data:` の行そのまま) を残す。request は gateway が受けた本文を残す。どの形の本文 (受けた形か upstream へ送った形か) を正とするかは未決 2。

- `--request-only` は request だけを書き、response を書かない。response だけを取る旗は持たない (要件が無い)
- 1 本に添えるメタデータは tap の 1 行 (DR-0017、`tap::Event`) と同じ欄 (ts / ns / model / route / status / origin / stream / 本文の大きさ / credential / cache の戦略) で、欄の定義を 2 つ持たない (起草者の判断)

### 8. 秘密のヘッダは落とす。述語は keepalive の控えと同じ (統括の補強案)

issue の案は Authorization を落とすことだが、gateway は控えに持ち回してはいけないヘッダの述語を既に 1 つ持っている (`gateway.rs` の `is_authorization`: `authorization` / `proxy-authorization` / `x-api-key` / `cookie`、DR-0027 決定 1)。ダンプも同じ述語で落とし、2 つ目の一覧を持たない (起草者の判断)。request の `Authorization` には ns 認証の token (DR-0030) が、upstream へ送る形には credential の token が入るので、どちらの形を残すにせよ落とす対象は同じ。

秘密以外のヘッダをどこまで残すかは未決 4。

### 9. 片付けは unit の起動時と `dump start` の時に、48 時間より古いものを消す (kawaz 要件 + 統括の補強案)

定期の timer を足さない。契機は unit が上がった時と `dump start` を受けた時の 2 つで、どちらも「ダンプに関心が向いた瞬間」だから、そこで掃けば溜まらない。

- 古さは mtime で判定し、48 時間より古いファイルを消す。空になったディレクトリも消す
- 掃く範囲は `dumps/` の下全体で、他の unit の分も含む (起草者の判断)。止まったままの unit は自分の分を掃けないので、どれかの unit が上がれば全部が片付く形にする。書いている途中のファイルは mtime が新しいので、兄弟 unit が書いている最中のものを消すことはない (1 本ごとのファイルでも追記でも成り立つ)
- `--for` の上限は未決 6。48 時間を超える時限を許すと、動いているダンプの初期のファイルを自分の `start` の片付けが消しうる

### 10. 置く位置は tap (DR-0017) と同じ (起草者の判断)

request は受け口 (`llm-gateway-server` の `messages` / `responses`) で本文を読み切り tap の控えを取る位置、response は relay の外の `exchange::observe` の包み (`BodyObservation`) で tap が応答のチャンクを控える位置に置く。relay は無変更 (DR-0011 の「本文を覗く層は relay の外に置く」)。

- tap と違い、response は終端で 1 件にまとめず通り過ぎるチャンクをそのまま書く。終端まで溜めると、上限いっぱいの本文が並列の数だけメモリに乗る
- tap が無い位置には置かない。認証差し替えの中継 (DR-0030 §2 の passthrough) は本文を読まずに流す口で、ダンプの対象にしない
- 動いていない間は 1 バイトも控えない。tap と同じく「誰も見ていなければ何もしない」で、中継の速度を落とさない

### 11. `dump status` の答え (起草者の判断)

```
llm-gateway dump status --unit unstable
{"unit":"unstable","active":true,
 "ns":"lab","request_only":false,
 "started_at":1792000000000,"until":1792000600000,
 "bytes":1234567,"max_bytes":209715200,"requests":12,
 "dir":"~/.local/state/llm-gateway/stats/dumps/unstable/lab"}
```

- `active` は常に出す。`false` のとき `ns` 以降の欄は出さず、代わりにその unit が上がってから最後に止まったダンプの `ended: {at, reason}` を出す (`reason` は `expired` / `max_bytes` / `stopped`)。容量で止まったのか時限で切れたのかを後から読めるようにする。一度も動いていなければ `ended` も出さない
- 時刻は DR-0012 の規約 (単位を名前に持たない数値 = Unix ミリ秒)。`until` を出すので残り時間の引き算は読む側がする
- `requests` は今回の `start` から書いた本数、`bytes` はその合計。`dir` は決定 6 の絶対パス (例は `~` で書いたが、実際には展開した値)
- `start` の答えも同じ形 (`active: true` の状態)。`stop` の答えは止めた直後の `status` と同じで、`ended.reason` が `stopped` になる

### 12. `daemon status` の各行にも、動いている間だけ出す (起草者の判断)

溜まり事故を防ぐのが要件の芯で、普段見る `daemon status` に出ていれば止め忘れに気づける。unit は `/llm-gateway/self` に `dump: {ns, until, bytes}` を載せ、監督者は DR-0028 決定 5 の経路 (`/self` の 1 回の問い合わせ) で各行に写す。動いていなければ欄ごと出さない。詳しい値は `dump status` が答える。

`/self` は認証の無い口だが、載せるのは ns の名前と時刻と大きさまでで、events (DR-0012) が既に流している以上の情報は含まない。本文の置き場も載せない (それは制御 socket 越しの `dump status` だけが答える)。

## 採らなかった案

- **設定ファイルに `[dump]` を書いて `daemon reload` で効かせる**: 消し忘れると常設になり、要件の「一時的に」と逆になる。書き換えを reload する手順も、時限で切れる形にできない (決定 2)
- **子の HTTP に `POST /llm-gateway/dump` を置く**: tap は `GET` で loopback 直結に限っているが、ダンプは本文をディスクへ書き残す操作で、外から届く面に置かない (決定 2)。DR-0032 が reload を HTTP に置かない構図と同じ
- **`daemon dump start|stop|status`**: `daemon` はプロセス操作の語で、ダンプは unit の生死を触らない。`usage` / `stats` が最上位に居るのと同じ区分に置く (決定 3)
- **tap (DR-0017) を `curl -N … > dump.jsonl` で取って済ませる**: 繋いでいるプロセスが生きている間しか取れず、`timeout` で時限にしても置き場・権限・片付けは取得者の手に残る。本文は既定 64 KiB で切り詰められ、`max_body` を明示する必要がある。メモリは応答の溜め込みより、購読者が止まった時に unit が抱える最大 64 件 × 64 MiB の request の控えが効く (実測: 購読者を SIGSTOP で止めると 9.7MB × 4 本で +39MB を持ち続け、再開時に `lagged` で切断)。tap は「その場で見る」口として残し、「数分だけ全量を残して後で突き合わせる」はダンプが受け持つ
- **定期の timer で 48 時間超を掃く**: 契機は unit の起動と `dump start` で足りる (決定 9)。間隔に根拠の無いループを足さない
- **全 ns を一括でダンプする旗**: 面の内容が別の面の作業で読まれる事故を減らす (決定 4)
- **ダンプ中の状態をディスクに書いて restart を跨いで続ける**: restart の後も溜まり続ける経路になる。timer は unit が持ち、restart で消える (決定 1)

## 影響

- `Request` (監督者) と `ControlRequest` (unit の制御口) に dump の語が増える。監督者と unit は別の binary でありうる (DR-0028 決定 2) ので、制御口が dump を知らない版の unit には `ok: false` を返す (DR-0032 の「版の混在」と同じ)
- `[stats] dir` の下に会話の本文が置かれる。keepalive の控えと同じ種類の機微で、置き場は 700 / 600 に閉じるが、ディスクを読める者には読める。ダンプしたファイルの扱い (リポや公開物に貼らない) は DR-0017 の運用ノートと同じ
- `/llm-gateway/self` と `daemon status` の行に `dump` の欄が増える (決定 12)。欄を知らない読み手はそのまま動く
- `dump` の最上位コマンドが増え、help / MANUAL / zsh completion に載る
- 認証差し替えの中継 (passthrough) は対象外 (決定 10)。中継の本文を残したくなったら、tap を中継に広げるかどうかと合わせて別に決める
- 常時の観測は OTel (docs/issue/2026-10-21-otel-observability-for-dev-environment.md) の側で、ダンプは調査用の一時手段に留める。ダンプに常設の機能 (検索・集計) を足さない

### 実装の段分け案

| 段 | 中身 | 完了条件 |
|---|---|---|
| 1 | unit 側: dump の状態 (ns / until / 上限 / 書いた量)、tap と同じ位置での書き出し、起動時と start 時の片付け、`/self` への露出 | 試験で、`until` を過ぎた 1 本と上限を超える 1 本が書かれず、48 時間より古いファイルだけが消える |
| 2 | 制御口と監督者の中継 (`ControlRequest` / `Request`)、答えの形 (決定 2・11) | 試験で、制御口を持たない unit に `ok: false` が返る |
| 3 | CLI `dump start|stop|status`、`--help`・zsh completion・MANUAL | `dump start --ns lab --for 10m` が置き場を答え、`daemon status` の行に `dump` が出て、10 分後に消える |

## 未決

Proposed のまま残す。実装の着手を止めるものではなく、着手した worker が判断して DR に書き足すか、kawaz が裁定する。

1. **容量上限の既定値**: issue の案は 200 MB 程度。並列 worker の本数と 1 本の大きさ (数百 KB) から決める
2. **どの形の本文を正とするか**: gateway が受けた形 (tap が控えている形、Claude Code の記録と突き合わせる用途) と upstream へ送った形 (`model` の解決、thinking の指示、cache_control の付け替え、thinking_as_text の変換を経た後。keepalive の控えが持つ形、DR-0024 の検証に要る) は違う。片方に決めるか両方を書くか
3. **1 ファイルの単位**: 1 本 = 1 ファイル (request と response を別ファイルにするか 1 つにするか、名前の付け方) か、ns ごとの jsonl に追記か。生の SSE をそのまま残すなら 1 本 1 ファイルが素直で、jsonl は本文を JSON 文字列として包むことになる。容量で止める粒度 (ファイル単位か行単位か) と、1 本 1 ファイルで数千ファイルになった時の扱いも絡む
4. **秘密以外のヘッダをどこまで残すか**: `anthropic-beta` や `x-claude-code-*` のように突き合わせに要るものと、`user-agent` のように要らないもの。全部残して決定 8 の述語で秘密だけ落とすのが最も単純
5. **events (DR-0012) と stats (DR-0011) にダンプ中を見せるか**: events に `dump` の種類を足すと ccmsg 等の恒常の読み手に揺れが及ぶ (DR-0017 が events を広げなかった理由)。`daemon status` と `/self` (決定 12) で足りるなら足さない
6. **`--for` の上限**: 48 時間の片付け (決定 9) との関係で、48 時間より短い上限を置くか。置くなら値
7. **`--unit` の `--all`**: Caddy は 11301 を優先し落ちていれば 11302 に回すので、調査中に両方へ流れうる。両 unit で同じ ns を同時にダンプする旗を持つか、2 回 `start` するか
8. **`daemon reload` で設定が差し替わった時**: 指した ns が新しい設定から消えたらその場で止めるか、`until` まで (書く相手が無いまま) 続けるか

## 関連

- docs/issue/2026-10-21-request-dump-with-timeout.md (本 DR の元。要件と補強案の正本)
- [DR-0017](DR-0017-debug-tap-endpoint.md) — tap。本文を控える位置とメタデータの欄を共有する。採らなかった案の「tap で済ませる」も参照
- [DR-0032](DR-0032-daemon-reload.md) — CLI → 監督者 → unit の制御 socket。dump の on/off はこの経路に乗る
- [DR-0028](DR-0028-daemon-service-subcommands.md) — `daemon` の体系と、走っている unit に聞く系の `--unit` の規則 (決定 6)
- [DR-0011](DR-0011-daily-usage-stats.md) — relay の外の tap と、writer を分けて排他を消す形
- [DR-0012](DR-0012-request-events.md) — 時刻は Unix ミリ秒。events に足すかは未決 5
- [DR-0027](DR-0027-keepalive-by-replay.md) — `[stats] dir` に本文を置く先例と、秘密のヘッダの述語
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) §12 / §13 — `[stats] dir` の下に置く時の作法 (700 / 600、ns の名前の制限) と壁時計の扱い
- [DR-0024](DR-0024-cache-strategy-and-keepalive.md) — prompt cache の検証に response が要る
- docs/issue/2026-10-21-otel-observability-for-dev-environment.md — 常時の観測は OTel 側、ダンプは一時手段
- claude-rules-personal の reference `cli-design/daemon-subcommands` — `daemon` の語の範囲
