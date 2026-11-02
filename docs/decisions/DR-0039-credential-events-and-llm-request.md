# DR-0039: credential の遷移と秘密の寿命を events の語彙に足し、`request` を `llm_request` に改名する

- Status: Proposed
- Date: 2026-10-21

## 背景

経路が上限に当たって候補から外れた (「この account は N 時間 M 分の間使えない」) こと、`until` が過ぎて候補に戻ったことは、今はリクエスト単位の知らせ (`request` の `credential` / `skipped`、DR-0012 / DR-0020) と tracing のログにしか出ない。`skipped` は「この 1 本で何を外したか」なので、外れている間は毎リクエストに同じ経路が並び、外れた瞬間も戻った瞬間も 1 件として読めない。会話が止まっている間に戻っても、次の 1 本が来るまで何も流れない。

認証 proxy としての知らせも同じ穴にある。refresh token が無効になった (`oauth.rs` の `refresh_failure_class` が `refresh_token_expired` / `refresh_token_invalidated` / `refresh_token_reused` と 400 / 401 を `ReloginRequired` に分類する) ことは usage の `auth` と log にしか出ず、気づくのは次の 1 本が全経路で断られてから。refresh token 自体の寿命は upstream が返さない (`expires_in` は access token の分) ので、切れる前に知る手段が無い。

kawaz の要件 (2026-10-21) のうち、本 DR が扱うのは credential の状態遷移 (締め出し / refresh の失敗 / 寿命の接近) を events の語彙に足すこと。選んだイベントを URL へ配達する `[[hooks]]` は [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) が扱い、本 DR の語彙を購読する。

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
- `credential` — その経路が使う認証情報の名前 (`routes.<name>.credential`、`[credentials.<name>]`)。認証情報を持たない経路 (relay) では欄ごと出さない。**events の全種で `credential` は認証情報の名前、経路は `route`** (決定 2 で `llm_request` / `response` / `skipped` も同じ 2 欄にする)
- `reason` — 印の理由。`skipped` / usage の `denials` と同じ語 (`limited` / `busy` / `org_not_allowed`)。`paced` は出ない (下記)
- `model` — 印の範囲。経路全体の印 (`Scope::Everything`) では欄ごと出さず、モデル単位・モデル群の印では usage の `denials` (DR-0020 §2) と同じくその名前 / パターン
- `until` — `credential_limited` だけ。この時刻までは候補にしない (印の `until` をミリ秒にしたもの)。`recovered` には無い (戻った時刻は `ts`)
- `by` — 何がその遷移を起こしたか。`limited` は `response` (断られた応答から印を付けた、`Preset::reject` / `Preset::admit`) か `probe` (枠照会の答えで印を引き直した、`Preset::apply_quota`)。`recovered` は `expired` (`until` が過ぎた) / `allowed` (通常の優先度の 2xx が印を消した、`Preset::allow`) / `probe` (枠照会が開いていると答えた) / `reload` (`daemon reload` で枠の変わった経路の印が引き継がれず消えた) / `login` / `refresh` (決定 3 の refresh の失敗が再ログイン / refresh で直った)
- `ns` / `fallback` — 契機が 1 本の転送 (`by: "response"` / `"allowed"`) の時だけ出す。`ns` はその 1 本の面、`fallback` はその 1 本で**断った経路の後ろに残っていた候補**の経路名を試される順に並べたもの (`credential_limited` だけ)。契機が枠照会なら両方とも欄ごと出さない

**`credential_recovered` は 1 つの形**で、印が効かなくなった時 (本決定) も refresh の失敗が直った時 (決定 3) も、常に `route` と `credential` の両方を持つ (relay の経路は印を持つが認証情報を持たないので、そこだけ `credential` が無い)。hooks の `match = { route = … }` (DR-0040 決定 1) がどちらにも当たる。

**どの変化で出すか**は、印そのものの前後比較ではなく、gateway が印とは別に持つ **「最後に知らせた状態」** (route × scope → 知らせた `reason`、または無し) との差で決める。印を触る全ての契機 (`RouteState` の `deny` / `allow` / `clear` / `apply`、`until` の到来を待つ timer、`daemon reload`) の**後**に、その経路の「今の有効な印」(`until > now`) と「最後に知らせた状態」を scope ごとに比べる:

| 最後に知らせた状態 | 今の有効な印 | 出すもの |
|---|---|---|
| その scope に無し | 有る | `credential_limited` |
| 有る | 無い | `credential_recovered` |
| 有る | 有る (`reason` が違う) | `credential_recovered` の後に `credential_limited` (印が置き換わった) |
| 有る | 有る (`until` だけ違う) | 出さない |

差が無ければ出さない。1 つの契機が複数の scope を変えれば (`allow(model)` はそのモデルを覆う印を全部消す、`denial.rs` の `scope.covers`) scope ごとに 1 件ずつ出す。印そのものの前後比較を採らないのは、期限切れの印が `deny` の `retain` (`denial.rs:189`) で黙って掃かれ、`daemon reload` でも黙って消えるため — 印を見るだけでは消えた瞬間を捕まえられない。「最後に知らせた状態」は印と同じくメモリだけで、経路が設定から消えればその経路の分は知らせずに落とす (無くなった経路を「戻った」とは言えない)。

`until` だけの更新で出さないのは、混雑 (`busy`) の印が 429 のたびに `now + 60` で置き直される (`RouteState::deny` は同じ範囲の期限を縮めない) ため。ここで出すとリクエストごとに繰り返すことになり、本決定の趣旨と逆になる。上限 (`limited`) の `until` が延びた (5h の次に 7d が塞がった) ことは usage の `denials` で読む。

**`recovered` は `until` の経過で出す (timer)**。timer は tokio の sleep (単調時計) で、機械のスリープ中は進まないので、復帰後の最初の判定で壁時計が `until` を過ぎていればその時に出す (DR-0037 §13 と同じ扱い)。timer 由来の知らせの `ts` は発火した時刻 (DR-0012 のとおり出来事の時刻) で、`until` ではない。候補に戻るのは `until` が過ぎた瞬間で (`Router::select` は `availability` の `until > now` で判定する)、そこで戻ったと知らせるのが事実に合う。最初の 2xx で出すと、会話が止まっている間は戻っても知らせが無く (戻ったことを見て作業を再開する用途に使えない)、2xx 自体は `llm_request` の `status` が既に伝えている。印を持つ経路の最も早い `until` まで寝る 1 本のタスクを置き、印の集合が変わるたびに起こし直す。印の実体を timer が消すのではない — 期限切れの印はこれまでどおり次に印を触った時に掃かれ、timer は上の比較を走らせて「有効でなくなった」知らせを出すだけ。`until` 到来の前に 2xx や枠照会で消えれば、そちらが `by: "allowed"` / `"probe"` で出し、timer は出し直さない (「最後に知らせた状態」が既に無しになっている)。

**`paced` は対象にしない**。pace_cap (DR-0019) は経路に印を残さず (`Route::paced_out` は判定のたびに計算する、DR-0019 §3)、namespace × モデル群ごとに結果が違うので「経路の状態の遷移」が無い。外れたことは `request` の `skipped` で読む (DR-0020 §2 と同じ切り分け)。

**`fallback` は契機の 1 本の候補列から取る**。`forward` が持つ経路の列は `Router::select` が返した順 (affinity、DR-0015 の 7d リセット順、DR-0018 の繰り上げ、DR-0035 の開始 account を反映済み) で、断った経路の後ろがそのまま「この後試される順」になる。列から名前を写すだけで計算が要らず、その 1 本の事実として正確。namespace の設定順から計算し直す形は採らない — 順序は会話 (affinity / thinking) に依るので、設定だけから「この ns の順」を 1 つに決められない。同じ理由で、`fallback` は**その 1 本の**順であって、同じ namespace の別の会話では違いうる。

**複数の namespace が同じ経路を使う時**は、印が経路の持ち物なので知らせも経路につき 1 回で、`ns` は契機の面だけが載る。別の面も同じ経路を失っているが、その面の名前は載らない (その面の候補列はこの 1 本に無い)。hooks の `match = { ns = … }` (DR-0040 決定 1) は契機の面に効くので、全部の面の知らせが要る hook は `ns` で絞らない。

**再起動では出ない**。印も「最後に知らせた状態」もメモリだけに持つ (DR-0009) ので、落ちた時点で両方が消えるが、それは「戻った」ではない。起動直後の 1 回の空振りが印を付け直し、そこで `credential_limited` が出る。`daemon reload` は契機に含める: 枠の変わった経路 (`Active::succeeding` が引き継がない経路) の印は消えて候補に戻るので、reload の後の比較が `credential_recovered` (`by: "reload"`) を出す。

### 2. 転送の知らせ `request` を `llm_request` に改名し、`type` を持たせる

SSE の `event:` 名と `Notice::name`、hooks の `on` に書く語を `request` から **`llm_request`** に変える。gateway は認証差し替えの中継 (`passthrough`、DR-0030 §2) も持つので、設定に書く語として「LLM の交換」であることが名前で分かるようにする。`response` / `cache_expired` / `passthrough` はそのまま。

合わせて、この種類にも **`"type":"llm_request"`** を先頭の欄として持たせる。`type` を持たないのはこの種類だけで (DR-0012: 先にあったので後から足すと読む側が壊れる)、名前を変える以上は読み側の変更が避けられないので、同じ機会に揃える。これで `untagged` の列で種類の順に依る読み分け (`events.rs` の `Notice` の並びの注記) が要らなくなり、webhook の配列の各件が `type` で見分けられる。

同じ機会に、**`credential` 欄を認証情報の名前にし、経路名は `route` に分ける**。今の `llm_request` / `response` の `credential` と `skipped[].credential` には経路名 (`route.name()`、`gateway.rs` の `Call::origin`) が入っていて、usage の 1 件の `name` と同じ「経路」を指している。決定 1 と決定 3 / 4 の種類が認証情報の名前を要る (印は経路に付き、refresh の失敗と寿命は認証情報に付く) ので、1 つの欄名に 2 つの意味を持たせず、全種で `route` = `[routes.<name>]`、`credential` = `[credentials.<name>]` に揃える。`llm_request` / `response` は `route` を常に出し、`credential` は経路が認証情報を持つ時だけ (relay の経路では欄ごと出さない)。`skipped` の要素は `{route, reason}`。router が組んだ 429 (`credential` に `"-"` を入れていた、DR-0020) は `route` に `"-"`。

公開契約の変更だが、`[webhook]` の廃止 (DR-0040 決定 3) で ccmsg 側の設定変更が同じリリースに入るので、このタイミングで一緒に変える。MANUAL の例と `docs/` の `event: request` の記述、`credential` の説明も全部置き換える。

### 3. refresh token が無効になったら `credential_refresh_failed` を 1 回、直ったら `credential_recovered`

```
event: credential_refresh_failed
id: 120
data: {"type":"credential_refresh_failed","ts":1785700000000,"seq":120,"boot":1785590000000,"credential":"claude-main","reason":"refresh_token_expired","routes":["claude-a","claude-a-low"],"login_path":"/llm-gateway/login/claude-main/start"}

event: credential_recovered
id: 131
data: {"type":"credential_recovered","ts":1785700600000,"seq":131,"boot":1785590000000,"route":"claude-a","credential":"claude-main","reason":"relogin_required","by":"login"}

event: credential_recovered
id: 132
data: {"type":"credential_recovered","ts":1785700600000,"seq":132,"boot":1785590000000,"route":"claude-a-low","credential":"claude-main","reason":"relogin_required","by":"login"}
```

- `credential` — 認証情報の名前 (`[credentials.<name>]`、`CredentialId`)。refresh の失敗は認証情報の状態 (`CredentialStore` の `auth`) なので、`credential_refresh_failed` は同じ認証情報を指す経路が何本あっても 1 回。`route` は無い (経路の状態ではない)
- `routes` — その認証情報を指す経路の名前 (設定順)。その経路が全部使えなくなるという意味で、hook の本文に書く用
- `reason` — 失敗の種類。`refresh_token_expired` / `refresh_token_invalidated` / `refresh_token_reused` (応答本文の語、`oauth.rs` の `refresh_failure_class` が見るもの) / `rejected` (語の無い 400 / 401)。出すのは `ReloginRequired` に分類された失敗だけで、`Degraded` (繋がらない、5xx、429、読めない応答) は一時的な失敗なので出さない (usage の `auth.status: "degraded"` と log に出る)
- `login_path` — 再ログインの Web ページ (DR-0023、`GET /llm-gateway/login/<name>/start`)。usage の `auth.login_path` と同じ値・同じ条件 (claude_oauth だけ。codex_oauth は Web login の対象外なので欄ごと出さない)。パスだけなのは gateway が自分の外向きの URL を知らないため (DR-0040 決定 4 の例)。CLI の手当て (`llm-gateway login --type …`) は log の文言にあり、イベントには載せない (本文に CLI の文を入れると受け手が整形できない)

**直った時の `credential_recovered` は決定 1 と同じ形**で、その認証情報を使う経路 1 本につき 1 件 (`route` + `credential`、`reason: "relogin_required"`、`routes` は持たない)。`by` は `login` (再ログインで直った) か `refresh` (refresh が通って直った。refresh token が無効な以上ほぼ無いが、通ったなら `login` と書くのは嘘になる)。

**出すかどうかは「最後に知らせた状態」との差で決める** (決定 1 と同じ構え)。認証情報ごとに「`credential_refresh_failed` を知らせたか (その `reason`)、または無し」を持ち、`AuthState.status` が変わりうる契機の後に比べる: 無し → 今 `ReloginRequired` なら `credential_refresh_failed`、知らせ済み → 今 `Ok` なら `credential_recovered`。`Degraded` はどちらにも動かさない (`ReloginRequired → Degraded → Ok` でも、知らせ済みのまま `Ok` を見た時に `recovered` が出る。`Degraded` 自体は直ったとも入ったとも言えない)。`ReloginRequired` のまま refresh を試し続けても (`acquire` のたびに `needs_refresh` → 失敗) 知らせた状態と同じなので繰り返さない。契機は 2 つ:

- `gateway_core::credential::refreshing` の `record_auth` (refresh の結果と `save_login` が通る口)。gateway-core は events を知らないので、llm-gateway 側の `CredentialStore` が `record_auth` の後に比べて publish する (配線は実装の判断)
- **版が進んだファイルの読み直し** (DR-0010 の版の照合) で、読み直した結果 `AuthStatus` が `Ok` になった時。兄弟の unit の Web login や CLI の `login` がファイルを直した時、この unit は `record_auth` を通らず版の進んだファイルを掴むだけなので、ここを契機に含めないと `recovered` が永久に出ない。`by` は `login`。例外は `refresh_token_reused` で入った状態 (兄弟の unit が先に refresh していた) からの読み直しで、直したのは兄弟の refresh なので `by: "refresh"`

**再起動では出ない**。`auth` も「最後に知らせた状態」もメモリだけで (再起動で `AuthState` は空)、起動後の最初の refresh の失敗が `credential_refresh_failed` を出す (決定 1 の印と同じ構え)。

### 4. 秘密の寿命はファイルの `expires_at` を読んで知らせるだけ (`credential_expiring` / `secret_expiring` / `*_expired`)

provider は秘密の寿命を返さない。OAuth の `expires_in` は access token の分で、credential ファイルの `expired` もその期限 (`payload` の欄は `claude_oauth` = `access_token` / `email` / `expired` / `refresh_token`、`codex_oauth` はそれに `account_id` / `id_token`、外側に `last_refresh` 等。login の時刻を持つ欄は無い)。codex の `id_token` は JWT で `iat` / `exp` を持つが、それは id token の寿命であって refresh token の寿命ではない。パススルーの固定の秘密 (`secrets/<id>.json`、DR-0030 §5) も同じで、発行元が示した期限を gateway は知らない。

**gateway は寿命を推定しない**。どちらの種類のファイルにも、トップ (`type` / `priority` / `disabled` と同じ層、`payload` の外) に optional の 2 欄を持たせ、gateway はそれを読んで知らせるだけ:

| 欄 | 書く人 | 意味 |
|---|---|---|
| `expires_at` | 人 | その秘密 (OAuth なら refresh token、static ならキー) の期限 (RFC 3339)。発行時に表示される期限を写す。無ければこの決定の知らせは出ない |
| `issued_at` | gateway (OAuth の login 時だけ) / 人 | 発行の時刻 (RFC 3339)。gateway が書くのは **OAuth の login (`save_login` と CLI の `login`) の時だけ**で、書き戻しは login が使う経路 (`.lock` を取り、置き場を読み直して土台にし、書き戻す、DR-0010) そのもの。**refresh の書き戻しでは進めない** (起点は login。ローテーション後の refresh token の寿命を gateway は知らないので、進めると経過率が根拠を失う)。static の秘密は人が書いた時だけ使い、gateway は書き足さない (「最初に読んだ時刻」は発行の時刻ではなく推定で、本決定の「推定しない」に反する)。人が書いてあればどちらの種類でもそれを使う |

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

- 種類は秘密の種類で分ける: LLM の認証情報は `credential_expiring` / `credential_expired` (欄 `credential` = `[credentials.<name>]`、`routes` は決定 3 と同じ)、パススルーの秘密は `secret_expiring` / `secret_expired` (欄 `secret` = 秘密の id、`upstreams` はその秘密を使う `[upstreams.<name>]` の名前)。欄名を揃えないのは、`passthrough` の知らせが `secret` / `upstream` の語を使っている (DR-0030) のに合わせるため
- `expires_at` はファイルの値を Unix ミリ秒にしたもの、`remaining_secs` は `expires_at` − `ts` (長さなので名前に単位、DR-0012)。`*_expired` に `remaining_secs` は無い
- **判定は独立した 1 時間周期のタスク**。起動時と 1 時間ごとに `expires_at` を持つ全ファイル (`[credentials]` の置き場と `[secret_store]` の置き場) を見る。`issued_at` があれば経過率 (`(now − issued_at) / (expires_at − issued_at)`) ≥ `expiry_ratio`、無ければ残り (`expires_at − now`) < `expiry_before` で `*_expiring`、`now` ≥ `expires_at` で `*_expired`。秒を争う知らせではないので、決定 1 の `until` の timer (秒の精度が要る) とは分ける。DR-0037 の鍵入れ替えの timer (`key_rotation::run`) には相乗りしない: その `wait` は `auth = "jwt"` の ns が無いと次の時刻を持たず `None` で寝続ける (`key_rotation.rs:104`) ので、寿命の判定を載せると ns の有無で動いたり止まったりする。スリープ跨ぎは DR-0037 §13 と同じ (単調時計で寝て、起きたら壁時計で比べ直す)
- **それぞれ 1 回だけ**。どのファイルについて `expiring` / `expired` を出したかを (id, `expires_at`) でメモリに持ち、同じ `expires_at` では繰り返さない。ファイルが書き換わって `expires_at` が伸びれば別の (id, `expires_at`) になり、状態が戻って再び知らせる。再起動で忘れるので、起動後の最初の周期で該当するものは 1 回出る (過ぎている事実を 1 回は伝える、決定 1 の印と同じ構え)
- `expires_at` が読めない (RFC 3339 でない) ファイルは `check` と周期タスクが warning で知らせ、その 1 件は飛ばす
- ローテーション後の寿命が login からでない provider では、人が `issued_at` を消して残り時間 (`expiry_before`) の判定に落とす。MANUAL にその手順を書く

## 採らなかった案

- **`switched` (経路が切り替わった) という独立イベント**: `credential_limited` と、その直後の `request` の `credential` で導出できる。`request` の `skipped` にも断った経路が並ぶ。1 件で読めることを 2 件に分けない
- **`credential_limited` を `until` の延長でも出す**: `busy` の印が 429 のたびに置き直されるので、リクエストごとに出る。「1 回だけ」が崩れる (決定 1)
- **`credential_recovered` を最初の 2xx だけで出す**: 会話が止まっている間は戻っても知らせが無い。timer で出す (決定 1)
- **`credential_limited` を namespace ごとに出す**: 印は経路の持ち物で面に依らない。面の数だけ同じ遷移が並び、「1 回だけ」が崩れる。面ごとの候補列は契機の 1 本にしか無い (決定 1)
- **`paced` も `credential_limited` に含める**: 印が無く、namespace × モデル群ごとに判定が違う。遷移として定義できない (決定 1)
- **`credential` 欄を経路名のまま残し、認証情報の名前を別の欄名で足す**: `llm_request` の既存の読み手を壊さないが、決定 3 / 4 で `credential` が認証情報を指す種類が増え、1 つの欄名が種類で意味を変える。改名 (決定 2) で読み手が変わる機会に `route` / `credential` に分ける
- **転送の知らせを `request` のまま残す**: `passthrough` とは別種なので区別は付くが、`on = ["request"]` と設定に書いた時に「中継は入らない」と名前から読めない。自己説明的でない語を設定の語彙にしない (決定 2)
- **`exchange` に改名する**: コードの span 名 (`exchange::request_span`) と一致するが、設定に書く語としては何と何の交換かが読めず意味が薄い。利用者の語彙 (LLM の 1 本) で付ける (決定 2)
- **`credential_refresh_failed` を refresh を試すたびに出す**: `ReloginRequired` のまま `acquire` のたびに失敗するので、リクエストごとに繰り返す。状態の遷移 (決定 3 の `record_auth` の前後) でだけ出す
- **`credential_refresh_failed` に CLI の手当ての文を載せる**: 受け手が整形できない文を契約に入れない。`reason` の語と `login_path` から受け手が組む
- **refresh token の寿命を `id_token` の `exp` や access token の `expired` から推定する**: どちらも別の token の寿命 (決定 4)。推定で鳴らすと外れた時に信用を失う。ファイルに書かれた期限だけを読み、無ければ出さない
- **寿命を設定で与える (`[credentials.<name>] refresh_lifetime = "90d"` と、起点を login / 最後の refresh から選ぶ `refresh_lifetime_from`)**: provider ごとの寿命の推定 (ローテーションする refresh token の寿命が最初のログインから数えるのか等) を gateway が持つことになり、実態を調べて既定を決める作業と、外れた時の修正が gateway 側に残る。期限はファイルの `expires_at` に人が写し、gateway は読むだけにする (決定 4)。OAuth と static の秘密で仕組みが 1 つになる
- **`credential_expiring` を定期的に繰り返す (1 日 1 回等)**: 間隔に根拠が無く、`expires_at` が変わらない限り同じ事実。1 回にし、しつこく鳴らしたいなら受け手 (bark の `level`) で決める

## 影響

- `Notice` に `CredentialLimited` / `CredentialRecovered` / `CredentialRefreshFailed` / `CredentialExpiring` が増える。SSE と hooks (DR-0040) の両方に流れる。知らない `type` を無視する読み手 (DR-0012) はそのまま動く
- 転送の知らせの名前が `request` → `llm_request` に変わり、`type` 欄が付く (決定 2)。**ccmsg 側は受け口の読み分け (`type` の無い件を転送として読む判定と、SSE の `event: request`) を `llm_request` に追従させる**。`[webhook]` → `[[hooks]]` の設定変更 (DR-0040) と同じリリースで入る
- `llm_request` / `response` の `credential` が経路名から認証情報の名前に変わり、経路名は `route` に移る。`skipped` の要素も `{route, reason}`。**ccmsg 側は経路を読む所を `route` に追従させる** (決定 2、改名と同じリリース)。`Event` / `Response` / `Skipped` と `Origin` に `route` が増え、`credential` は `Option`
- `Notice` の全種が `type` を持つので `untagged` の読み分けを `type` で引ける形にできる (読み戻しの順序依存が消える)
- `credential_refresh_failed` / `credential_recovered (relogin_required)` の配線: llm-gateway の `CredentialStore` が認証情報ごとの「最後に知らせた状態」を持ち、`record_auth` の後と版の進んだファイルの読み直しの後に `AuthStatus` と比べて publish する。gateway-core は events を知らないまま
- 認証情報と秘密のファイルのトップに optional の `issued_at` / `expires_at` が増える (決定 4、`StoredCredential` の共通欄)。`issued_at` を gateway が書くのは OAuth の login の書き戻しだけで、refresh の書き戻しと static の置き場には新しい書き込みの経路は増えない。読み戻しは欄が無くても壊れない
- 設定に `[notice]` (`expiry_ratio` / `expiry_before`) が増える。`check` が `ratio` の範囲 (0 < r ≤ 1) を検証する。`daemon reload` で変えられる
- `Notice` に `SecretExpiring` / `SecretExpired` / `CredentialExpired` も増える (決定 4)。寿命の判定を 1 時間ごとに回す独立のタスクが増える (`key_rotation::run` とは別)
- 経路 × scope ごとの「最後に知らせた状態」を持つ器と、`until` の到来を待つ timer のタスクが増える。印を触る口 (`deny` / `allow` / `clear` / `apply`) から events へ届ける配線は、`Preset` が events を知らない (DR-0014 §3) ので、呼び出し側 (`gateway.rs` の `reject` / `allow` / `apply_quota` の周り) で印を触った後に「最後に知らせた状態」と比べて publish する形になる。timer と `daemon reload` の後も同じ比較を走らせる。`fallback` は `forward` の経路の列から写す
- 混雑 (`busy`) の印はモデル単位で 60 秒なので `credential_limited` / `recovered` が短い間隔で対になって出る。通知に使う hook は `match = { reason = … }` で絞る (DR-0040 決定 4)
- MANUAL の events の節に新しい種類 (決定 1・3・4 の 7 種) と `llm_request` への改名を足す

## 未決

(無し。kawaz 2026-10-30 に次を裁定: `upstream_sent` は `llm_request` の `ts` (upstream へ送り始めた瞬間、DR-0012) そのものなので足さない。`until` が延びた時の知らせ、人が読める時刻の欄、`login_path` の絶対 URL 化はいずれも持たない。`recovered` の timer がスリープを跨いだ時は DR-0037 §13 と同じく、復帰後に壁時計と `until` を比べて出す (決定 1 に記載))

## 関連

- [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) — 本 DR の語彙を購読して、選んだイベントを整形して URL へ配達する `[[hooks]]`
- [DR-0012](DR-0012-request-events.md) — events の契約 (`seq` / `boot`、Unix ミリ秒、未知の `type` は無視、転送の邪魔をしない)
- [DR-0020](DR-0020-denial-reason-visibility.md) — events の語彙を増やす判断の前例。`skipped` (リクエスト単位) と本 DR の遷移 (状態単位) の切り分け、`paced` を印に残さない理由、usage の `denials` の形
- [DR-0009](DR-0009-credential-denial-fallback.md) — 印の付け外し (断られ方ごとの `until` と範囲、2xx で消す、枠照会で引き直す、印はメモリだけ)
- [DR-0015](DR-0015-routing-priority-and-reset-aware-ordering.md) / [DR-0018](DR-0018-spend-down-priority.md) / [DR-0035](DR-0035-judge-thinking-crossing-by-session-sources.md) — 候補列の順序が会話ごとに動く理由 (`fallback` を契機の 1 本から写す根拠)
- [DR-0019](DR-0019-pace-cap.md) — pace_cap は印を残さない (`paced` を対象にしない根拠)
- [DR-0036](DR-0036-low-priority-requests-wait-instead-of-switching.md) — 低優先度の 1 本は印を付けも外しもしない (遷移の契機にならない)
- [DR-0030](DR-0030-general-purpose-auth-gateway.md) §2 / §5 — 認証差し替えの中継 (`passthrough`、決定 2 で `llm_request` と名前で分ける理由) と秘密のファイル (決定 4 の `expires_at` / `issued_at` を持たせる先)
- [DR-0023](DR-0023-web-login-endpoint.md) — Web login の口 (`login_path` の指す先、claude_oauth だけ)
- [DR-0010](DR-0010-credential-cross-process-lock.md) — 認証情報ファイルの書き換えと版 (login が `issued_at` を書き戻す経路、版の進んだファイルの読み直しが決定 3 の契機になる根拠)
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) §13 — timer のスリープ跨ぎの扱い (決定 1 の `until` の timer と決定 4 の周期タスクが同じ構えを取る)
- [DR-0007](DR-0007-usage-visibility.md) — usage の `auth` (`status` / `login_path`)。決定 3 のイベントと同じ値を出す口
- [DR-0032](DR-0032-daemon-reload.md) — reload で差し替えられる欄の区分 (`[notice]`)
- [DR-0014](DR-0014-target-architecture-provider-preset.md) §3 — `Preset` / `RouteState` は events を知らない (配線の位置)
