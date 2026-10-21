# DR-0039: credential の遷移を events に足し、`[[hooks]]` で選んだイベントを整形して配達する (`[webhook]` は廃止)

- Status: Proposed
- Date: 2026-10-21

## 背景

経路が上限に当たって候補から外れた (「この account は N 時間 M 分の間使えない」) こと、`until` が過ぎて候補に戻ったことは、今はリクエスト単位の知らせ (`request` の `credential` / `skipped`、DR-0012 / DR-0020) と tracing のログにしか出ない。`skipped` は「この 1 本で何を外したか」なので、外れている間は毎リクエストに同じ経路が並び、外れた瞬間も戻った瞬間も 1 件として読めない。会話が止まっている間に戻っても、次の 1 本が来るまで何も流れない。

通知を出す口も無い。`[webhook]` (DR-0012) は ccmsg の受け口 1 つのための形で、送り先に `/webhook/llm-gateway` を固定で足し、合言葉の既定パスに ccmsg の名前が入っている (`config.rs` の `default_token_file`)。gateway が受け手を知っている密結合で、別の受け手 (bark 等の通知) に同じイベントを流すには受け手ごとに同じ形の口を増やすことになる。

認証 proxy としての知らせも同じ穴にある。refresh token が無効になった (`oauth.rs` の `refresh_failure_class` が `refresh_token_expired` / `refresh_token_invalidated` / `refresh_token_reused` と 400 / 401 を `ReloginRequired` に分類する) ことは usage の `auth` と log にしか出ず、気づくのは次の 1 本が全経路で断られてから。refresh token 自体の寿命は upstream が返さない (`expires_in` は access token の分) ので、切れる前に知る手段が無い。

kawaz の要件 (2026-10-21) は 3 つ。credential の状態遷移 (締め出し / refresh の失敗 / 寿命の接近) を events の語彙に足すこと、gateway 自身が選んだイベントで URL を叩く `[[hooks]]` を足して通知 (bark 等) と ccmsg の配達を同じ枠組みに載せること、テンプレートはプアに固定すること (`{{field}}` / `{{field.field2}}` / `{{event}}` だけ、if / loop / 整形は持たない、無い欄は黙って空にしない)。hooks は本体を阻害しない (reference `proxy-dump-design` の「実装の原則」と同じ: 透過、非ブロッキング、失敗は log のみ)。

## 決定

### 1. 印の遷移を `credential_limited` / `credential_recovered` として 1 回だけ流す

締め出しの印 (`crate::denial::RouteState` の `marks`、`Scope` ごとに 1 つの `Denial { until, reason, scope }`) が**新しく効き始めた**時に `credential_limited`、**効かなくなった**時に `credential_recovered` を events (DR-0012) へ流す。リクエストごとに繰り返さない — 外れている間に何本来ても、印が変わらなければ何も出ない。

```
event: credential_limited
id: 58
data: {"type":"credential_limited","ts":1785600000000,"seq":58,"boot":1785590000000,"route":"claude-a","credential":"claude-main","reason":"limited","until":1785618060000,"by":"response","ns":"personal","fallback":["claude-b","claude-c"]}

event: credential_recovered
id: 97
data: {"type":"credential_recovered","ts":1785618060000,"seq":97,"boot":1785590000000,"route":"claude-a","credential":"claude-main","reason":"limited","by":"expired"}
```

- `type` / `ts` / `seq` / `boot` — DR-0012 の規則 (印を持つ種類、Unix ミリ秒、`request` / `response` / `cache_expired` / `passthrough` と同じ 1 つの列で番号を振る)
- `route` — 経路の名前 (`[routes.<name>]`)。この種類の主キー。印は経路ごとに持つ (`router::Active::presets` が経路名で 1 つ、namespace を跨いで共有、`daemon reload` で枠が同じ経路は引き継ぐ) ので、同じ認証情報を指す経路が 2 つあれば印も知らせも別々に出る
- `credential` — その経路が使う認証情報の名前 (`routes.<name>.credential`、`[credentials.<name>]`)。認証情報を持たない経路 (relay) では欄ごと出さない。**events の全種で `credential` は認証情報の名前、経路は `route`** (決定 7 で `llm_request` / `response` / `skipped` も同じ 2 欄にする)
- `reason` — 印の理由。`skipped` / usage の `denials` と同じ語 (`limited` / `busy` / `org_not_allowed`)。`paced` は出ない (下記)
- `model` — 印の範囲。経路全体の印 (`Scope::Everything`) では欄ごと出さず、モデル単位・モデル群の印では usage の `denials` (DR-0020 §2) と同じくその名前 / パターン
- `until` — `credential_limited` だけ。この時刻までは候補にしない (印の `until` をミリ秒にしたもの)。`recovered` には無い (戻った時刻は `ts`)
- `by` — 何がその遷移を起こしたか。`limited` は `response` (断られた応答から印を付けた、`Preset::reject` / `Preset::admit`) か `probe` (枠照会の答えで印を引き直した、`Preset::apply_quota`)。`recovered` は `expired` (`until` が過ぎた) / `allowed` (通常の優先度の 2xx が印を消した、`Preset::allow`) / `probe` (枠照会が開いていると答えた) / `login` (決定 8 の refresh の失敗が再ログインで直った)
- `ns` / `fallback` — 契機が 1 本の転送 (`by: "response"` / `"allowed"`) の時だけ出す。`ns` はその 1 本の面、`fallback` はその 1 本で**断った経路の後ろに残っていた候補**の経路名を試される順に並べたもの (`credential_limited` だけ)。契機が枠照会なら両方とも欄ごと出さない

**どの印の変化で出すか**は、印を触る 1 箇所 (`RouteState` の `deny` / `allow` / `clear` / `apply`) で前後の「有効な印」(`until > now`) を scope ごとに比べて決める:

| 前 | 後 | 出すもの |
|---|---|---|
| その scope に有効な印が無い | 有る | `credential_limited` |
| 有る | 無い | `credential_recovered` |
| 有る | 有る (`reason` が違う) | `credential_recovered` の後に `credential_limited` (印が置き換わった) |
| 有る | 有る (`until` だけ違う) | 出さない |

`until` だけの更新で出さないのは、混雑 (`busy`) の印が 429 のたびに `now + 60` で置き直される (`RouteState::deny` は同じ範囲の期限を縮めない) ため。ここで出すとリクエストごとに繰り返すことになり、本決定の趣旨と逆になる。上限 (`limited`) の `until` が延びた (5h の次に 7d が塞がった) ことは usage の `denials` で読む (未決 5)。

**`recovered` は `until` の経過で出す (timer)**。候補に戻るのは `until` が過ぎた瞬間で (`Router::select` は `availability` の `until > now` で判定する)、そこで戻ったと知らせるのが事実に合う。最初の 2xx で出すと、会話が止まっている間は戻っても知らせが無く (戻ったことを見て作業を再開する用途に使えない)、2xx 自体は `request` の `status` が既に伝えている。印を持つ経路の最も早い `until` まで寝る 1 本のタスクを置き、印の集合が変わるたびに起こし直す。印の実体を timer が消すのではない — 期限切れの印はこれまでどおり次に印を触った時に掃かれ (`deny` の `retain`)、timer は「有効でなくなった」知らせを出すだけ。`until` 到来の前に 2xx や枠照会で消えれば、そちらが `by: "allowed"` / `"probe"` で出し、timer は出し直さない。

**`paced` は対象にしない**。pace_cap (DR-0019) は経路に印を残さず (`Route::paced_out` は判定のたびに計算する、DR-0019 §3)、namespace × モデル群ごとに結果が違うので「経路の状態の遷移」が無い。外れたことは `request` の `skipped` で読む (DR-0020 §2 と同じ切り分け)。

**`fallback` は契機の 1 本の候補列から取る**。`forward` が持つ経路の列は `Router::select` が返した順 (affinity、DR-0015 の 7d リセット順、DR-0018 の繰り上げ、DR-0035 の開始 account を反映済み) で、断った経路の後ろがそのまま「この後試される順」になる。列から名前を写すだけで計算が要らず、その 1 本の事実として正確。namespace の設定順から計算し直す形は採らない — 順序は会話 (affinity / thinking) に依るので、設定だけから「この ns の順」を 1 つに決められない。同じ理由で、`fallback` は**その 1 本の**順であって、同じ namespace の別の会話では違いうる。

**複数の namespace が同じ経路を使う時**は、印が経路の持ち物なので知らせも経路につき 1 回で、`ns` は契機の面だけが載る。別の面も同じ経路を失っているが、その面の名前は載らない (その面の候補列はこの 1 本に無い)。hooks の `match = { ns = … }` (決定 2) は契機の面に効くので、全部の面の知らせが要る hook は `ns` で絞らない。

**再起動では出ない**。印はメモリだけに持つ (DR-0009) ので、落ちた時点で全部の印が消えるが、それは「戻った」ではない。起動直後の 1 回の空振りが印を付け直し、そこで `credential_limited` が出る。`daemon reload` で枠の変わった経路 (`Active::succeeding` が引き継がない経路) の印も同じく黙って消え、新しい印が `limited` を出す。

### 2. `[[hooks]]`: 選んだイベントを整形して URL へ配る

```toml
[[hooks]]
on = ["credential_limited", "credential_recovered"]   # 配るイベントの種類 (必須、1 つ以上)
match = { route = "claude-*", reason = "limited" }        # 任意: top-level の欄の値で絞る (AND、`*` glob)
url = "https://bark.example/bark-admin/push"           # 必須。テンプレート可
method = "POST"                                        # 既定 POST
headers = { "Content-Type" = "application/json", Authorization = "Bearer {{secret:bark}}" }   # 値はテンプレート可
body = '{"device":{"owner":"kawaz"},"title":"{{route}} is {{reason}}","body":"until {{until}}; next {{fallback}}","group":"llm-gateway"}'   # テンプレート
timeout = "5s"                                         # 既定 5s
follow_redirects = true                                # 既定 true
```

| 欄 | 意味 |
|---|---|
| `on` | 配るイベントの種類。SSE の `event:` 名と同じ語 (`llm_request` (決定 7) / `response` / `cache_expired` / `passthrough` / `credential_limited` / `credential_recovered` / `credential_refresh_failed` (決定 8) / `credential_expiring` / `credential_expired` / `secret_expiring` / `secret_expired` (決定 9))。知らない語は `check` が断る |
| `match` | 任意の絞り。`{ <欄> = "<値>" }` で、イベントの **top-level の欄**の値が一致する 1 件だけ配る。値は完全一致か `*` glob (`chatgpt-*`)、複数の欄は AND。欄は列挙せずどの top-level 欄でも書ける (`ns` / `credential` / `secret` / `upstream` / `reason` / `origin` / `status` …) ので、欄が増えても文法は変わらない。値は**文字列だけ** (TOML の数値 / 真偽は `check` が断る)。文字列以外の欄 (`status` の数値、`aborted` の真偽) は JSON の表記 (`"200"` / `"true"`) で比べ、配列 / object の欄には当たらない。欄を持たないイベント (`cache_expired` に `credential` は無い) は、その欄を書いた hook には当たらない。`credential` はどの種類でも認証情報の名前、経路は `route` (決定 1・7)。`check` は欄名を検証しない (どの種類にどの欄があるかは events の契約で、増える) |
| `url` | 送り先。`http` / `https` だけ。テンプレートを展開した結果を URL として読む |
| `method` | `GET` / `POST` / `PUT` / `PATCH` / `DELETE`。既定 `POST` |
| `headers` | 名前 → 値。値はテンプレート。`Content-Type` を書かなければ `body` がある時だけ `application/json` を付ける |
| `body` | テンプレート。無ければ本文なし |
| `timeout` | 1 回の送信を諦めるまで。既定 `5s` |
| `follow_redirects` | 3xx の `Location` を**同じ method と本文で**追う (既定 `true`、最大 3 回、同じ origin に限る)。cold start で 302 を返す受け口 (bark-admin) を 1 回で通すため。ブラウザの意味論 (POST の 302 を GET に変える) は採らない — 送り先は設定で決めた 1 つの受け口で、結果ページへ移る用途が無い。別 origin への `Location` は追わず失敗にする (合言葉を別の相手に送らない) |

配達の原則 (本体を阻害しない):

- **events の購読者の 1 つ**として走る (決定 6)。転送側は `Events::publish` で流すだけで、hook の有無も結果も知らない
- **hook ごとに別タスクと bounded queue**。購読タスクが受けた 1 件を、当たる hook の列 (既定 64 件) へ入れる。列が一杯ならその 1 件を**落として数え**、転送も購読も待たない。溢れるのは受け口が遅い時で、遅れて届く知らせに価値が無いのは SSE と同じ (DR-0012)
- 落とした数は `/llm-gateway/self` の `hooks.dropped` (全 hook の合計、起動からの累積、0 でも欄は出す) と `daemon status` の各行に出る。購読の段で追いつけずに落ちた分は従来どおり `events.dropped` に入る (2 段で数える: 購読の遅れと配達の遅れは別の事象)
- **失敗は log だけ**。2xx 以外の応答、通信失敗、timeout、テンプレートの展開失敗はすべて warning で、再送の仕組みは持たない。例外として**通信失敗と 5xx と timeout は 1 回だけ送り直す** (bark-admin の cold start が 1〜2 秒かかる実測に合わせる。4xx は同じ結果になるので送り直さない)。連続失敗は最初の 1 回だけ warning を出し、復帰時に失敗回数を記録する (`webhook.rs` と同じ形)
- **log に合言葉と本文を出さない**。展開前のテンプレート (`{{secret:…}}` のまま) と状態コードまで。受け口の応答本文も記録しない
- 1 件 1 配達。複数件をまとめる窓は持たない (「採らなかった案」)。接続は reqwest の pool で使い回す
- `[[hooks]]` は `daemon reload` で差し替えられる (購読タスクを止めて組み直す)。配達の途中の 1 件は送り切る

### 3. テンプレートの文法

`url` / `headers` の値 / `body` に書ける。文法は 3 種類の置き換えだけで、if / loop / 整形 / 既定値は持たない:

| 書き方 | 埋まるもの |
|---|---|
| `{{name}}` | イベントの欄 `name` の値 |
| `{{a.b}}` | 入れ子の欄 (object の中の object)。配列は辿らない (`{{fallback.0}}` は無い欄) |
| `{{event}}` | イベント丸ごと (JSON object)。`event` は予約語で、イベントの欄名には使わない |
| `{{secret:<id>}}` | `[secret_store]` の `<id>.json` の値 (`[upstreams] secret = "<id>"` と同じ id、同じ置き場、同じ最小形 `{"type":"static","payload":{"value":"…"}}`)。発火のたびに読む (書き換えは次の発火から効く、DR-0010 の版の照合) |

**埋め方は置かれた文脈で決まる**。`body` は、テンプレートの各 `{{…}}` を `null` に置き換えたテキストが JSON として読めるなら **JSON 文脈**:

- JSON の文字列リテラルの中に置かれた `{{…}}` は、値を**文字列にして JSON エスケープ**して埋める。文字列はそのまま、数値 / 真偽は 10 進 / `true` `false`、`null` は `null` の 4 文字、配列 / object はその JSON テキスト
- 文字列の外 (値の位置) に置かれた `{{…}}` は、値を **JSON 値として**埋める。文字列なら引用符付き、数値はそのまま、配列 / object はその JSON
- `body` 全体が `{{event}}` なら、イベントの JSON がそのまま本文になる (生 POST)。`[{{event}}]` なら 1 件の配列

`null` に置き換えたテキストが JSON として読めなければ**素の文脈**で、どの `{{…}}` も値を文字列にして (エスケープなしで) 埋める (文字列はそのまま、他は JSON テキスト)。`url` と `headers` は常に素の文脈で、URL エンコードも持たない (未決 4)。

**無い欄はエラー**。イベントにその欄が無い (`cache_expired` に `credential` は無い、`fallback` は契機が枠照会の時は無い、`until` は `recovered` に無い) か、`{{secret:<id>}}` が読めなければ、その hook の**その 1 件は発火せず** warning に残す (どの hook の、どのイベントの、どの欄が無かったか)。黙って空文字にすると、受け手が「値が空だった」と「テンプレートが間違っている」を区別できない。欄ごと出ないことのある欄 (DR-0012 の `skip_serializing_if` の欄) をテンプレートに書くと、その欄の無い 1 件は配達されない。全部の欄が要るなら `{{event}}` で丸ごと送る。

`{{` を文字として書く手段は持たない。閉じの無い `{{`、知らない予約語 (`{{foo:bar}}`) は `check` が断る。

### 4. `[webhook]` の廃止と ccmsg の受け口の書き方

`[webhook]` を設定から取り除く。設定に残っていれば `check` (と `daemon reload` の検証、起動時の読み込み) が **`[[hooks]]` への書き換え例を出して断る** (`[secrets] type` / `dir` の移転と同じ形、`config.rs` の `secret_specs`)。1 リリースの猶予を置くかは未決 1。

ccmsg の受け口は hooks でこう書く:

```toml
[[hooks]]
on = ["llm_request", "response", "cache_expired", "passthrough"]
url = "http://127.0.0.1:8643/webhook/llm-gateway"
headers = { Authorization = "Bearer {{secret:ccmsg-webhook}}" }
body = "[{{event}}]"
```

- 送り先はパスまで書く。gateway がパスを足さない
- 合言葉は `[secret_store]` の `ccmsg-webhook.json` に置く (`{"type":"static","payload":{"value":"<token>"}}`)。1 行テキストのファイルを直接指す形は持たず、秘密の置き場を 1 つにする
- `[{{event}}]` で受け口の契約 (常に配列、DR-0012) を保つ。1 件 1 配達になるので、短時間に起きた分が 1 通にまとまることは無くなる (各件が自分の `seq` / `boot` を持つのは同じ)
- 複数の受け口は `[[hooks]]` を受け口の数だけ書く。全部へ同じ知らせを流し gateway は宛先を選ばない、1 つが落ちていても他への配達を止めない、はそのまま成り立つ (hook ごとに別タスク)

### 5. bark への通知の例

```toml
[[hooks]]
on = ["credential_limited"]
match = { reason = "limited" }
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","title":"{{route}} ({{credential}}): {{reason}}","body":"until {{until}}, next: {{fallback}}","level":"timeSensitive"}'

[[hooks]]
on = ["credential_recovered"]
match = { reason = "limited" }
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","title":"{{route}} recovered","body":"{{by}}"}'

[[hooks]]
on = ["credential_refresh_failed"]
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","level":"timeSensitive","title":"{{credential}} needs login","body":"{{reason}}; routes {{routes}}","url":"https://gw.example{{login_path}}"}'

[[hooks]]
on = ["credential_expiring", "credential_expired"]
url = "https://bark.example/bark-admin/push"
body = '{"device":{"owner":"kawaz"},"group":"llm-gateway","title":"{{credential}}: {{type}}","body":"expires_at {{expires_at}}; routes {{routes}}"}'
```

- 1 つ目の `{{credential}}` は認証情報を持たない経路 (relay) では無い欄なので、その経路の 1 件は発火しない (決定 3)。relay も鳴らすなら `{{route}}` だけにする
- `reason` を絞るのは、混雑 (`busy`) の 60 秒の退避で鳴らさないため。`org_not_allowed` も鳴らしたいなら `[[hooks]]` をもう 1 つ書く (`match` の値は 1 つで、OR は hook を分けて表す)
- 再ログインの催促は bark の `url` (タップで開く) に login ページを入れる。gateway は自分の外向きの URL を知らない (`login_path` はパスだけ、決定 8) ので、ホストは hook を書く人が前に付ける。`login_path` の無い認証情報 (codex_oauth、DR-0023 の対象外) ではこの hook は発火しない (決定 3) ので、codex も鳴らしたいなら `url` を外した hook を別に書く
- `fallback` は JSON 文字列の中に置かれているので `["claude-b","claude-c"]` のテキストが入る。枠照会が付けた印 (`by: "probe"`) には `fallback` が無く、この hook のその 1 件は発火しない (決定 3)。欠けても鳴らしたいなら `fallback` を本文から外す
- `until` は Unix ミリ秒のまま入る。人が読む形への整形はテンプレートに無く、イベントにも載せない (DR-0012 の「同じ瞬間を 2 通りで並べない」)。通知で読める時刻が要るかは未決 6
- bark-admin の cold start の 302 は `follow_redirects` (既定) が同じ POST で追う

### 6. events (SSE) は変えず、hooks は購読者の 1 つ

`/llm-gateway/events` の形・配り方 (`tokio::sync::broadcast`、256 件、追いつけない見物人の分は落とす) は変えない。新しい 7 種 (決定 1・8・9) は `Notice` の種類として増え、SSE にも同じ `seq` で流れる。events の語彙が正本で、hooks はその上の配達 (絞り + 整形 + 送信) に徹する。hooks のためにイベントの欄を足す・変えることはしない — hook で要る値は、まず events の契約として正しいかを問う。

### 7. 転送の知らせ `request` を `llm_request` に改名し、`type` を持たせる

SSE の `event:` 名と `Notice::name`、hooks の `on` に書く語を `request` から **`llm_request`** に変える。gateway は認証差し替えの中継 (`passthrough`、DR-0030 §2) も持つので、設定に書く語として「LLM の交換」であることが名前で分かるようにする。`response` / `cache_expired` / `passthrough` はそのまま。

合わせて、この種類にも **`"type":"llm_request"`** を先頭の欄として持たせる。`type` を持たないのはこの種類だけで (DR-0012: 先にあったので後から足すと読む側が壊れる)、名前を変える以上は読み側の変更が避けられないので、同じ機会に揃える。これで `untagged` の列で種類の順に依る読み分け (`events.rs` の `Notice` の並びの注記) が要らなくなり、webhook の配列の各件が `type` で見分けられる。

同じ機会に、**`credential` 欄を認証情報の名前にし、経路名は `route` に分ける**。今の `llm_request` / `response` の `credential` と `skipped[].credential` には経路名 (`route.name()`、`gateway.rs` の `Call::origin`) が入っていて、usage の 1 件の `name` と同じ「経路」を指している。決定 1 と決定 8 / 9 の種類が認証情報の名前を要る (印は経路に付き、refresh の失敗と寿命は認証情報に付く) ので、1 つの欄名に 2 つの意味を持たせず、全種で `route` = `[routes.<name>]`、`credential` = `[credentials.<name>]` に揃える。`llm_request` / `response` は `route` を常に出し、`credential` は経路が認証情報を持つ時だけ (relay の経路では欄ごと出さない)。`skipped` の要素は `{route, reason}`。router が組んだ 429 (`credential` に `"-"` を入れていた、DR-0020) は `route` に `"-"`。

公開契約の変更だが、`[webhook]` の廃止 (決定 4) で ccmsg 側の設定変更が同じリリースに入るので、このタイミングで一緒に変える。MANUAL の例と `docs/` の `event: request` の記述、`credential` の説明も全部置き換える。

### 8. refresh token が無効になったら `credential_refresh_failed` を 1 回、直ったら `credential_recovered`

```
event: credential_refresh_failed
id: 120
data: {"type":"credential_refresh_failed","ts":1785700000000,"seq":120,"boot":1785590000000,"credential":"claude-main","reason":"refresh_token_expired","routes":["claude-a","claude-a-low"],"login_path":"/llm-gateway/login/claude-main/start"}

event: credential_recovered
id: 131
data: {"type":"credential_recovered","ts":1785700600000,"seq":131,"boot":1785590000000,"credential":"claude-main","reason":"relogin_required","by":"login","routes":["claude-a","claude-a-low"]}
```

- `credential` — 認証情報の名前 (`[credentials.<name>]`、`CredentialId`)。refresh の失敗は認証情報の状態 (`CredentialStore` の `auth`) なので、同じ認証情報を指す経路が何本あっても 1 回。`route` は無い (経路の状態ではない)
- `routes` — その認証情報を指す経路の名前 (設定順)。その経路が全部使えなくなるという意味で、hook の本文に書く用。`credential_recovered` (`reason: "relogin_required"`) にも同じ値
- `reason` — 失敗の種類。`refresh_token_expired` / `refresh_token_invalidated` / `refresh_token_reused` (応答本文の語、`oauth.rs` の `refresh_failure_class` が見るもの) / `rejected` (語の無い 400 / 401)。出すのは `ReloginRequired` に分類された失敗だけで、`Degraded` (繋がらない、5xx、429、読めない応答) は一時的な失敗なので出さない (usage の `auth.status: "degraded"` と log に出る)
- `login_path` — 再ログインの Web ページ (DR-0023、`GET /llm-gateway/login/<name>/start`)。usage の `auth.login_path` と同じ値・同じ条件 (claude_oauth だけ。codex_oauth は Web login の対象外なので欄ごと出さない)。パスだけなのは gateway が自分の外向きの URL を知らないため (決定 5 の例)。CLI の手当て (`llm-gateway login --type …`) は log の文言にあり、イベントには載せない (本文に CLI の文を入れると受け手が整形できない)

**出す場所は 1 つ**: `gateway_core::credential::refreshing` の `record_auth` (認証の観測を `AuthState` に書く唯一の口。refresh の結果と `save_login` の両方がここを通る)。前の `AuthState.status` と新しい `status` を比べ、`ReloginRequired` 以外 → `ReloginRequired` で `credential_refresh_failed`、`ReloginRequired` → `Ok` で `credential_recovered` (`by: "login"`。refresh が通って直ることは refresh token が無効な以上ほぼ無いが、通ったなら同じ口で出る)。`ReloginRequired` のまま refresh を試し続けても (`acquire` のたびに `needs_refresh` → 失敗) 状態が変わらないので繰り返さない。`Degraded` ↔ `ReloginRequired` は失敗の種類が変わっただけで、`Degraded` → `ReloginRequired` は失敗状態に**入った**として出し、逆は出さない (`Degraded` は直ったとは言えない)。gateway-core は events を知らないので、`record_auth` が前の状態を返し、llm-gateway 側の `CredentialStore` が差を見て publish する (配線は実装の判断)。

**再起動では出ない**。`auth` はメモリだけで (再起動で `AuthState` は空)、起動後の最初の refresh の失敗が `credential_refresh_failed` を出す (決定 1 の印と同じ構え)。

### 9. 秘密の寿命はファイルの `expires_at` を読んで知らせるだけ (`credential_expiring` / `secret_expiring` / `*_expired`)

provider は秘密の寿命を返さない。OAuth の `expires_in` は access token の分で、credential ファイルの `expired` もその期限 (`payload` の欄は `claude_oauth` = `access_token` / `email` / `expired` / `refresh_token`、`codex_oauth` はそれに `account_id` / `id_token`、外側に `last_refresh` 等。login の時刻を持つ欄は無い)。codex の `id_token` は JWT で `iat` / `exp` を持つが、それは id token の寿命であって refresh token の寿命ではない。パススルーの固定の秘密 (`secrets/<id>.json`、DR-0030 §5) も同じで、発行元が示した期限を gateway は知らない。

**gateway は寿命を推定しない**。どちらの種類のファイルにも、トップ (`type` / `priority` / `disabled` と同じ層、`payload` の外) に optional の 2 欄を持たせ、gateway はそれを読んで知らせるだけ:

| 欄 | 書く人 | 意味 |
|---|---|---|
| `expires_at` | 人 | その秘密 (OAuth なら refresh token、static ならキー) の期限 (RFC 3339)。発行時に表示される期限を写す。無ければこの決定の知らせは出ない |
| `issued_at` | gateway | 発行の時刻 (RFC 3339)。OAuth は login (`save_login` と CLI の `login`) で入れ、**refresh の書き戻しでは進めない** (起点は login。ローテーション後の refresh token の寿命を gateway は知らないので、進めると経過率が根拠を失う)。static は最初に読んだ時に無ければ書き足す。書き戻しは既存の経路 (`.lock` を取り、置き場を読み直して土台にし、書き戻す、DR-0010) を使う。人が書いてあればそれを使う |

```toml
[notice]
expiry_ratio = 0.9     # issued_at があれば、経過率がここを超えたら知らせる。既定 0.9
expiry_before = "7d"   # issued_at が無ければ、残りがこれを切ったら知らせる。既定 7d
```

```
event: credential_expiring
id: 140
data: {"type":"credential_expiring","ts":1792000000000,"seq":140,"boot":1785590000000,"credential":"claude-main","routes":["claude-a"],"expires_at":1792777600000,"remaining_secs":777600}

event: secret_expiring
id: 141
data: {"type":"secret_expiring","ts":1792000000000,"seq":141,"boot":1785590000000,"secret":"xai","upstreams":["api.x.ai"],"expires_at":1792604800000,"remaining_secs":604800}

event: credential_expired
id: 205
data: {"type":"credential_expired","ts":1792777600000,"seq":205,"boot":1785590000000,"credential":"claude-main","routes":["claude-a"],"expires_at":1792777600000}
```

- 種類は秘密の種類で分ける: LLM の認証情報は `credential_expiring` / `credential_expired` (欄 `credential` = `[credentials.<name>]`、`routes` は決定 8 と同じ)、パススルーの秘密は `secret_expiring` / `secret_expired` (欄 `secret` = 秘密の id、`upstreams` はその秘密を使う `[upstreams.<name>]` の名前)。欄名を揃えないのは、`passthrough` の知らせが `secret` / `upstream` の語を使っている (DR-0030) のに合わせるため
- `expires_at` はファイルの値を Unix ミリ秒にしたもの、`remaining_secs` は `expires_at` − `ts` (長さなので名前に単位、DR-0012)。`*_expired` に `remaining_secs` は無い
- **判定は周期タスク**。DR-0037 の鍵入れ替えの timer (`key_rotation::run`) に相乗りし、1 時間ごとに `expires_at` を持つ全ファイル (`[credentials]` の置き場と `[secret_store]` の置き場) を見る。`issued_at` があれば経過率 (`(now − issued_at) / (expires_at − issued_at)`) ≥ `expiry_ratio`、無ければ残り (`expires_at − now`) < `expiry_before` で `*_expiring`、`now` ≥ `expires_at` で `*_expired`。秒を争う知らせではないので、決定 1 の `until` の timer (秒の精度が要る) とは分ける
- **それぞれ 1 回だけ**。どのファイルについて `expiring` / `expired` を出したかを (id, `expires_at`) でメモリに持ち、同じ `expires_at` では繰り返さない。ファイルが書き換わって `expires_at` が伸びれば別の (id, `expires_at`) になり、状態が戻って再び知らせる。再起動で忘れるので、起動後の最初の周期で該当するものは 1 回出る (過ぎている事実を 1 回は伝える、決定 1 の印と同じ構え)
- `expires_at` が読めない (RFC 3339 でない) ファイルは `check` と周期タスクが warning で知らせ、その 1 件は飛ばす
- ローテーション後の寿命が login からでない provider では、人が `issued_at` を消して残り時間 (`expiry_before`) の判定に落とす。MANUAL にその手順を書く

## 採らなかった案

- **`[webhook]` を残して `[[hooks]]` と併存させる**: 同じイベントを URL へ送る口が 2 つになり、合言葉の読み方・失敗の数え方・timeout を両方で保守する。`[webhook]` は `[[hooks]]` の 1 つの書き方 (決定 4) で表せるので、残す理由が無い
- **テンプレートに if / loop / 整形 (時刻の書式、配列の結合) を持つ**: 文法が増えるほど `check` で検証できる範囲が狭まり、展開の失敗が増える。整形は受け手 (bark の本文は人が読む、ccmsg は JSON を読む) の仕事で、gateway が持つと受け手の数だけ要望が積もる
- **無い欄を空文字にする**: 受け手が「値が空だった」と「テンプレートの欄名が間違っている」を区別できない。設定の誤りが黙って通る口を作らない
- **`switched` (経路が切り替わった) という独立イベント**: `credential_limited` と、その直後の `request` の `credential` で導出できる。`request` の `skipped` にも断った経路が並ぶ。1 件で読めることを 2 件に分けない
- **配達を同期にする (転送の中で送る)**: 受け口の遅れが転送の遅れになる。DR-0012 の「転送の邪魔をしない」のとおり、配達は切り離す
- **ccmsg 側で bark に流す (gateway に hooks を持たない)**: 通知の経路が ccmsg の生死に依り、ccmsg が落ちている時に限って「経路が尽きた」知らせが届かない。gateway 自身が知っていることは gateway から出す
- **`credential_limited` を `until` の延長でも出す**: `busy` の印が 429 のたびに置き直されるので、リクエストごとに出る。「1 回だけ」が崩れる (決定 1)
- **`credential_recovered` を最初の 2xx だけで出す**: 会話が止まっている間は戻っても知らせが無い。timer で出す (決定 1)
- **`credential_limited` を namespace ごとに出す**: 印は経路の持ち物で面に依らない。面の数だけ同じ遷移が並び、「1 回だけ」が崩れる。面ごとの候補列は契機の 1 本にしか無い (決定 1)
- **`paced` も `credential_limited` に含める**: 印が無く、namespace × モデル群ごとに判定が違う。遷移として定義できない (決定 1)
- **複数件を 1 通にまとめる窓 (`[webhook]` の 200 ms / 100 件) を hooks に持つ**: テンプレートが 1 件の形で書かれているので、まとめるには配列の文法 (loop) が要る。1 件 1 配達で接続は増えるが、pool で使い回し、落とす数は `hooks.dropped` で見える
- **`credential` 欄を経路名のまま残し、認証情報の名前を別の欄名で足す**: `llm_request` の既存の読み手を壊さないが、決定 8 / 9 で `credential` が認証情報を指す種類が増え、1 つの欄名が種類で意味を変える。改名 (決定 7) で読み手が変わる機会に `route` / `credential` に分ける
- **hook に名前 (`name`) を持たせる**: log と `/self` の識別は `url` (展開前) で足りる。識別のためだけの欄を足さない
- **転送の知らせを `request` のまま残す**: `passthrough` とは別種なので区別は付くが、`on = ["request"]` と設定に書いた時に「中継は入らない」と名前から読めない。自己説明的でない語を設定の語彙にしない (決定 7)
- **`exchange` に改名する**: コードの span 名 (`exchange::request_span`) と一致するが、設定に書く語としては何と何の交換かが読めず意味が薄い。利用者の語彙 (LLM の 1 本) で付ける (決定 7)
- **`credential_refresh_failed` を refresh を試すたびに出す**: `ReloginRequired` のまま `acquire` のたびに失敗するので、リクエストごとに繰り返す。状態の遷移 (決定 8 の `record_auth` の前後) でだけ出す
- **`credential_refresh_failed` に CLI の手当ての文を載せる**: 受け手が整形できない文を契約に入れない。`reason` の語と `login_path` から受け手が組む
- **refresh token の寿命を `id_token` の `exp` や access token の `expired` から推定する**: どちらも別の token の寿命 (決定 9)。推定で鳴らすと外れた時に信用を失う。ファイルに書かれた期限だけを読み、無ければ出さない
- **寿命を設定で与える (`[credentials.<name>] refresh_lifetime = "90d"` と、起点を login / 最後の refresh から選ぶ `refresh_lifetime_from`)**: provider ごとの寿命の推定 (ローテーションする refresh token の寿命が最初のログインから数えるのか等) を gateway が持つことになり、実態を調べて既定を決める作業と、外れた時の修正が gateway 側に残る。期限はファイルの `expires_at` に人が写し、gateway は読むだけにする (決定 9)。OAuth と static の秘密で仕組みが 1 つになる
- **`credential_expiring` を定期的に繰り返す (1 日 1 回等)**: 間隔に根拠が無く、`expires_at` が変わらない限り同じ事実。1 回にし、しつこく鳴らしたいなら受け手 (bark の `level`) で決める
- **hooks の絞りを `ns` / `credential` / `reason` の固定の欄で持つ**: 欄が増えるたび (`secret` / `upstream` / `origin` …) に設定の文法が増える。top-level の欄に対する一致 (`match`) なら events の語彙が増えても文法は変わらない (決定 2)

## 影響

- `Notice` に `CredentialLimited` / `CredentialRecovered` / `CredentialRefreshFailed` / `CredentialExpiring` が増える。SSE と hooks の両方に流れる。知らない `type` を無視する読み手 (DR-0012) はそのまま動く
- 転送の知らせの名前が `request` → `llm_request` に変わり、`type` 欄が付く (決定 7)。**ccmsg 側は受け口の読み分け (`type` の無い件を転送として読む判定と、SSE の `event: request`) を `llm_request` に追従させる**。`[webhook]` → `[[hooks]]` の設定変更と同じリリースで入る
- `llm_request` / `response` の `credential` が経路名から認証情報の名前に変わり、経路名は `route` に移る。`skipped` の要素も `{route, reason}`。**ccmsg 側は経路を読む所を `route` に追従させる** (決定 7、改名と同じリリース)。`Event` / `Response` / `Skipped` と `Origin` に `route` が増え、`credential` は `Option`
- `Notice` の全種が `type` を持つので `untagged` の読み分けを `type` で引ける形にできる (読み戻しの順序依存が消える)
- `credential_refresh_failed` / `credential_recovered (relogin_required)` の配線: `refreshing::Inner::record_auth` が前の `AuthStatus` を返し、llm-gateway の `CredentialStore` が差を見て publish する。gateway-core は events を知らないまま
- 認証情報と秘密のファイルのトップに optional の `issued_at` / `expires_at` が増える (決定 9、`StoredCredential` の共通欄)。`issued_at` は gateway が login / refresh の書き戻しと、static の最初の読み込みで書き足す (static の書き戻しが新しく増える: 今は読むだけの置き場に、`.lock` を取って 1 度だけ書く経路が要る)。読み戻しは欄が無くても壊れない
- 設定に `[notice]` (`expiry_ratio` / `expiry_before`) が増える。`check` が `ratio` の範囲 (0 < r ≤ 1) を検証する。`daemon reload` で変えられる
- `Notice` に `SecretExpiring` / `SecretExpired` / `CredentialExpired` も増える (決定 9)。`key_rotation::run` が 1 時間ごとに寿命の判定を兼ねる
- `RouteState` に「前後の有効な印の差」を返す口と、`until` の到来を待つ timer のタスクが増える。印を触る 4 つの口 (`deny` / `allow` / `clear` / `apply`) から events へ届ける配線は、`Preset` が events を知らない (DR-0014 §3) ので、呼び出し側 (`gateway.rs` の `reject` / `allow` / `apply_quota` の周り) で publish する形になる。`fallback` は `forward` の経路の列から写す
- `webhook.rs` と `config::Webhook` が消え、`hooks.rs` と `config::Hook` が増える。`[webhook]` を書いた設定は `check` で断られ、書き換え例が出る。ccmsg の合言葉ファイル (`webhook-llm-gateway.token`) は `[secret_store]` の `ccmsg-webhook.json` へ写す (運用の手順、MANUAL に書く)
- `[secret_store]` は `[upstreams]` が空だと開かない (`passthrough.rs`)。`{{secret:…}}` を使う hook があれば開く
- `/llm-gateway/self` と `daemon status` の各行に `hooks.dropped` が増える。`[[hooks]]` は `daemon reload` で変えられる欄にする (`[webhook]` は restart を要する欄)
- 1 件 1 配達になるので、受け口への接続数は `[webhook]` の窓でまとめていた時より増える。ccmsg の受け口は配列を受けるのでそのまま動く
- 混雑 (`busy`) の印はモデル単位で 60 秒なので `credential_limited` / `recovered` が短い間隔で対になって出る。通知に使う hook は `match = { reason = … }` で絞る (決定 5)
- MANUAL の events の節に 2 種を足し、webhook の節を hooks に置き換える。`docs/architecture-overview` の webhook の記述も同様

## 未決

1. **`[webhook]` の猶予**: 1 リリースの間は読み替えて警告だけにするか、最初から断るか。読み替える場合 `token_file` の 1 行テキストを `[secret_store]` へ写す処理を持つか
2. **`upstream_sent` (upstream へ送った時点のイベント) の要否**: `request` の `ts` が送り始めの時刻なので、今の語彙で足りているかもしれない
3. **`match` で `prefix` / `session_id` を絞る使い方を認めるか**: 文法上は top-level の欄なので書けるが、会話ごとに別の受け口へ、は DR-0012 の「gateway は宛先を選ばない」と逆の形になる。`check` で断るか、書けるが勧めない (MANUAL に書かない) か
4. **dotted path で配列の添字 (`{{fallback.0}}`) を許すか**、`url` に埋める値の URL エンコードを持つか
5. **`until` が延びた時 (5h の次に 7d が塞がった) に知らせるか**: `limited` の印に限って `until` の変化で出す案があるが、決定 1 の表を `reason` で場合分けすることになる
6. **通知で人が読める時刻**: イベントに載せると DR-0012 の規約に反し、テンプレートに整形を持つと決定 3 に反する。`until` と `ts` の差を長さの欄 (`for_secs`) として載せる案は、欄が派生値で DR-0012 の「足し算をさせない」に沿うが、`recovered` には無い欄になる
7. **サスペンド跨ぎの timer**: 寝ている間に壁時計が `until` を過ぎた時、復帰後にいつ `recovered` を出すか (tokio の sleep は単調時計、DR-0037 §13 と同じ問題)
8. **hook ごとの列の長さ (既定 64)** と、`hooks.dropped` を hook ごとに出すか合計だけか
9. **`credential_refresh_failed` の `login_path` を絶対 URL にするか**: gateway に外向きの URL の設定 (`[server] public_url` 等) を足せば `login_url` が出せるが、欄を 1 つ出すためだけに設定を増やすことになる。今は hook の側でホストを前に付ける (決定 5)

## 関連

- [DR-0012](DR-0012-request-events.md) — events の契約 (`seq` / `boot`、Unix ミリ秒、未知の `type` は無視、転送の邪魔をしない)。`[webhook]` の決定はこの DR で置き換わる
- [DR-0020](DR-0020-denial-reason-visibility.md) — events の語彙を増やす判断の前例。`skipped` (リクエスト単位) と本 DR の遷移 (状態単位) の切り分け、`paced` を印に残さない理由、usage の `denials` の形
- [DR-0009](DR-0009-credential-denial-fallback.md) — 印の付け外し (断られ方ごとの `until` と範囲、2xx で消す、枠照会で引き直す、印はメモリだけ)
- [DR-0015](DR-0015-routing-priority-and-reset-aware-ordering.md) / [DR-0018](DR-0018-spend-down-priority.md) / [DR-0035](DR-0035-judge-thinking-crossing-by-session-sources.md) — 候補列の順序が会話ごとに動く理由 (`fallback` を契機の 1 本から写す根拠)
- [DR-0019](DR-0019-pace-cap.md) — pace_cap は印を残さない (`paced` を対象にしない根拠)
- [DR-0036](DR-0036-low-priority-requests-wait-instead-of-switching.md) — 低優先度の 1 本は印を付けも外しもしない (遷移の契機にならない)
- [DR-0030](DR-0030-general-purpose-auth-gateway.md) §2 / §3 / §5 — 認証差し替えの中継 (`passthrough`、決定 7 で `llm_request` と名前で分ける理由) と `[secret_store]` と秘密の id (`{{secret:<id>}}` の参照先)
- [DR-0023](DR-0023-web-login-endpoint.md) — Web login の口 (`login_path` の指す先、claude_oauth だけ)
- [DR-0010](DR-0010-credential-cross-process-lock.md) — 認証情報ファイルの書き換えと版 (`issued_at` を書き戻す経路、`{{secret:<id>}}` の読み直し)
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) — 鍵入れ替えの timer (`key_rotation::run`)。決定 9 の周期判定が相乗りする
- [DR-0007](DR-0007-usage-visibility.md) — usage の `auth` (`status` / `login_path`)。決定 8 のイベントと同じ値を出す口
- [DR-0032](DR-0032-daemon-reload.md) — reload で差し替えられる欄の区分
- [DR-0014](DR-0014-target-architecture-provider-preset.md) §3 — `Preset` / `RouteState` は events を知らない (配線の位置)
- claude-rules-personal の reference `proxy-dump-design` (`model` の「実装の原則」) — 透過 / 非ブロッキング / 失敗は log のみ
- claude-rules-personal の reference `bark-push` — bark-admin の送り方 (cold start の 302、宛先は `device` タグ)
