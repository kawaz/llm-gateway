# DR-0039: credential の失効と秘密の寿命を events の語彙に足し、`request` を `llm_request` に改名する

- Status: Proposed
- Date: 2026-10-21

## 背景

認証 proxy としての知らせに穴がある。refresh token が無効になった (`oauth.rs` の `refresh_failure_class` が `refresh_token_expired` / `refresh_token_invalidated` / `refresh_token_reused` と 400 / 401 を `ReloginRequired` に分類する) ことは usage の `auth` と log にしか出ず、気づくのは次の 1 本が全経路で断られてから。refresh token 自体の寿命は upstream が返さない (`expires_in` は access token の分) ので、切れる前に知る手段が無い。

転送の知らせの語彙にも 2 つのずれがある。種類の名前 `request` は、gateway が認証差し替えの中継 (`passthrough`、DR-0030 §2) も持つ今、何の request かを名前で言っていない。`credential` の欄には経路の名前 (`route.name()`、`gateway.rs` の `Call::origin`) が入っていて、`GET /llm-gateway/sessions` (DR-0042) が `routes[] {route, credential}` で経路名と認証情報名を分けて答えるのと食い違う。

kawaz の要件 (2026-10-21) のうち、本 DR が扱うのは credential の失効 (refresh の失敗) と秘密の寿命の接近を events の語彙に足すことと、転送の知らせの名前と欄を揃えること。経路が枠 (`limited`) で塞がった・戻ったことは、取りに来る側が DR-0042 の `routes[].limited` を hook のたびに読めば分かるので、本 DR は扱わない。選んだイベントを URL へ配達する `[[hooks]]` は [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) が扱い (bark 等 push でしか届かない相手が要る時に起こす)、本 DR の語彙を購読する。

## 決定

### 1. 転送の知らせ `request` を `llm_request` に改名し、`type` を持たせる

SSE の `event:` 名と `Notice::name`、hooks の `on` に書く語を `request` から **`llm_request`** に変える。読むのは SSE を見る人と、種類を選んで購読する設定 (`[[hooks]]` の `on`) を書く人で、名前だけで「LLM の交換」であって中継 (`passthrough`) ではないと分かるようにするため。`response` / `cache_expired` / `passthrough` はそのまま。

合わせて、この種類にも **`"type":"llm_request"`** を先頭の欄として持たせる。`type` を持たないのはこの種類だけで (DR-0012: 先にあったので後から足すと読む側が壊れる)、名前を変える以上は読み側の変更が避けられないので、同じ機会に揃える。これで `untagged` の列で種類の順に依る読み分け (`events.rs` の `Notice` の並びの注記) が要らなくなり、種類を混ぜて受け取る読み手 (webhook の配列、SSE を記録したもの) が各件を `type` で見分けられる。

MANUAL の例と `docs/` の `event: request` の記述も全部置き換える。

### 2. 全種で `credential` は認証情報の名前、経路の名前は `route` に分ける

今の `llm_request` / `response` の `credential` と `skipped[].credential` には経路名 (`route.name()`) が入っていて、usage の 1 件の `name` と同じ「経路」を指している。決定 3 / 4 の種類は認証情報の名前を要る (refresh の失敗と寿命は認証情報に付く) ので、1 つの欄名に 2 つの意味を持たせず、全種で **`route` = `[routes.<name>]`、`credential` = `[credentials.<name>]`** に揃える。読むのは events と `GET /llm-gateway/sessions` (DR-0042) の両方を見る側で、`routes[] {route, credential}` と同じ名前を同じ意味で読めるようにするため (DR-0042 影響: 本決定が入れば全部が同じ名前になる)。

- `llm_request` / `response` は `route` を常に出し、`credential` は経路が認証情報を持つ時だけ (relay の経路では欄ごと出さない)
- `skipped` の要素は `{route, reason}`。router が組んだ 429 (`credential` に `"-"` を入れていた、DR-0020) は `route` に `"-"`
- 決定 3 / 4 の種類は `credential` (認証情報) を主キーに持ち、`route` は持たない (経路の状態ではない)。その認証情報を指す経路の名前は `routes` (配列) で添える

決定 1 の改名で読み手が変わる機会に入れる。`credential` の説明を MANUAL と `docs/` で置き換える。

### 3. refresh token が無効になったら `credential_refresh_failed` を 1 回 (対の知らせは無い)

```
event: credential_refresh_failed
id: 120
data: {"type":"credential_refresh_failed","ts":1785700000000,"seq":120,"boot":1785590000000,"credential":"claude-main","reason":"refresh_token_expired","routes":["claude-a","claude-a-low"],"login_path":"/llm-gateway/login/claude-main/start"}
```

読むのは「再ログインという人の操作が要る」と知りたい人 (SSE を見ている運用者、または hooks の受け手) で、次の 1 本が全経路で断られる前に気づくため。

- `type` / `ts` / `seq` / `boot` — DR-0012 の規則 (印を持つ種類、Unix ミリ秒、`llm_request` / `response` / `cache_expired` / `passthrough` と同じ 1 つの列で番号を振る)
- `credential` — 認証情報の名前 (`[credentials.<name>]`、`CredentialId`)。refresh の失敗は認証情報の状態 (`CredentialStore` の `auth`) なので、同じ認証情報を指す経路が何本あっても 1 回。`route` は無い (決定 2)
- `routes` — その認証情報を指す経路の名前 (設定順)。その経路が全部使えなくなるという意味で、hook の本文に書く用
- `reason` — 失敗の種類。`refresh_token_expired` / `refresh_token_invalidated` / `refresh_token_reused` (応答本文の語、`oauth.rs` の `refresh_failure_class` が見るもの) / `rejected` (語の無い 400 / 401)。出すのは `ReloginRequired` に分類された失敗だけで、`Degraded` (繋がらない、5xx、429、読めない応答) は一時的な失敗なので出さない (usage の `auth.status: "degraded"` と log に出る)
- `login_path` — 再ログインの Web ページ (DR-0023、`GET /llm-gateway/login/<name>/start`)。usage の `auth.login_path` と同じ値・同じ条件 (claude_oauth だけ。codex_oauth は Web login の対象外なので欄ごと出さない)。パスだけなのは gateway が自分の外向きの URL を知らないため。CLI の手当て (`llm-gateway login --type …`) は log の文言にあり、イベントには載せない (本文に CLI の文を入れると受け手が整形できない)

**出す場所は 1 つ**: `gateway_core::credential::refreshing` の `record_auth` (認証の観測を `AuthState` に書く唯一の口。refresh の結果と `save_login` の両方がここを通る)。前の `AuthState.status` と新しい `status` を比べ、`ReloginRequired` 以外 → `ReloginRequired` の時だけ出す。`ReloginRequired` のまま refresh を試し続けても (`acquire` のたびに `needs_refresh` → 失敗) 状態が変わらないので繰り返さない。`Degraded` → `ReloginRequired` は失敗状態に**入った**として出す。gateway-core は events を知らないので、`record_auth` が前の状態を返し、llm-gateway 側の `CredentialStore` が差を見て publish する (配線は実装の判断)。

**直った時の知らせは出さない**。この知らせの価値は「人の操作 (再ログイン) が要る」と伝えることにあり、直すのは人なので、直ったことは操作した本人が知っている。gateway 側でも次の `llm_request` が通れば分かる。直った契機は `save_login` のほかに兄弟の unit や CLI の `login` がファイルを直す経路もあり、それを漏れなく拾うには状態の器が要るが、見返りが無い。

**再起動では出ない**。`auth` はメモリだけで (再起動で `AuthState` は空)、起動後の最初の refresh の失敗が `credential_refresh_failed` を出す。過ぎている事実を 1 回は伝える構え。

### 4. 秘密の寿命はファイルの `expires_at` を読んで知らせるだけ (`credential_expiring` / `secret_expiring` / `*_expired`)

provider は秘密の寿命を返さない。OAuth の `expires_in` は access token の分で、credential ファイルの `expired` もその期限 (`payload` の欄は `claude_oauth` = `access_token` / `email` / `expired` / `refresh_token`、`codex_oauth` はそれに `account_id` / `id_token`、外側に `last_refresh` 等。login の時刻を持つ欄は無い)。codex の `id_token` は JWT で `iat` / `exp` を持つが、それは id token の寿命であって refresh token の寿命ではない。パススルーの固定の秘密 (`secrets/<id>.json`、DR-0030 §5) も同じで、発行元が示した期限を gateway は知らない。

読むのは秘密を更新する人 (再ログイン、キーの再発行) で、切れて経路が止まる前に予定を立てるため。

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
- **判定は独立した 1 時間周期のタスク**。起動時と 1 時間ごとに `expires_at` を持つ全ファイル (`[credentials]` の置き場と `[secret_store]` の置き場) を見る。`issued_at` があれば経過率 (`(now − issued_at) / (expires_at − issued_at)`) ≥ `expiry_ratio`、無ければ残り (`expires_at − now`) < `expiry_before` で `*_expiring`、`now` ≥ `expires_at` で `*_expired`。DR-0037 の鍵入れ替えの timer (`key_rotation::run`) には相乗りしない: その `wait` は `auth = "jwt"` の ns が無いと次の時刻を持たず `None` で寝続ける (`key_rotation.rs:104`) ので、寿命の判定を載せると ns の有無で動いたり止まったりする。スリープ跨ぎは DR-0037 §13 と同じ (単調時計で寝て、起きたら壁時計で比べ直す)
- **それぞれ 1 回だけ**。どのファイルについて `expiring` / `expired` を出したかを (id, `expires_at`) でメモリに持ち、同じ `expires_at` では繰り返さない。ファイルが書き換わって `expires_at` が伸びれば別の (id, `expires_at`) になり、状態が戻って再び知らせる。再起動で忘れるので、起動後の最初の周期で該当するものは 1 回出る (過ぎている事実を 1 回は伝える、決定 3 と同じ構え)
- `expires_at` が読めない (RFC 3339 でない) ファイルは `check` と周期タスクが warning で知らせ、その 1 件は飛ばす
- ローテーション後の寿命が login からでない provider では、人が `issued_at` を消して残り時間 (`expiry_before`) の判定に落とす。MANUAL にその手順を書く

## 採らなかった案

- **枠で塞がった・戻った経路を `credential_limited` / `credential_recovered` として events に流す**: `GET /llm-gateway/sessions/{session_id}` (DR-0042 決定 1) が系列ごとに `routes[] {route, credential?, limited?: {until}}` を答えるので、取りに来る側は hook のたびに読んで前回と比べれば、塞がった経路も戻った経路も分かる。押す形は印が付く・消える全ての契機 (印を置く `deny`、期限切れの掃き、枠照会の引き直し、`daemon reload`、再起動) に配線が要り、`until` を待つ timer を持っても印が別の経路で消えた分は取りこぼす。pull にはその問題が無い (印が消えれば次の答えに載らないだけ)。枠の印の見せ方を `limited` だけに絞る線も DR-0042 が引いている
- **`switched` (経路が切り替わった) という独立イベント**: `llm_request` の `route` と `skipped` (断った経路) で読める。1 件で読めることを 2 件に分けない
- **`credential` 欄を経路名のまま残し、認証情報の名前を別の欄名で足す**: `llm_request` の既存の読み手を壊さないが、決定 3 / 4 で `credential` が認証情報を指す種類が増え、1 つの欄名が種類で意味を変える。DR-0042 の `routes[]` は既に `route` / `credential` で分けている。改名 (決定 1) で読み手が変わる機会に分ける (決定 2)
- **転送の知らせを `request` のまま残す**: `passthrough` とは別種なので区別は付くが、`on = ["request"]` と設定に書いた時に「中継は入らない」と名前から読めない。自己説明的でない語を設定の語彙にしない (決定 1)
- **`exchange` に改名する**: コードの span 名 (`exchange::request_span`) と一致するが、設定に書く語としては何と何の交換かが読めず意味が薄い。利用者の語彙 (LLM の 1 本) で付ける (決定 1)
- **`credential_refresh_failed` の対として直った時に知らせを出す**: 直すのは人で、直ったことは本人が知っている。gateway でも次の `llm_request` で分かる。直った契機 (兄弟の unit や CLI の `login` がファイルを直す) を漏れなく拾うには状態の器が要り、見返りが無い (決定 3)
- **`credential_refresh_failed` を refresh を試すたびに出す**: `ReloginRequired` のまま `acquire` のたびに失敗するので、リクエストごとに繰り返す。状態の遷移 (決定 3 の `record_auth` の前後) でだけ出す
- **`credential_refresh_failed` に CLI の手当ての文を載せる**: 受け手が整形できない文を契約に入れない。`reason` の語と `login_path` から受け手が組む
- **refresh token の寿命を `id_token` の `exp` や access token の `expired` から推定する**: どちらも別の token の寿命 (決定 4)。推定で鳴らすと外れた時に信用を失う。ファイルに書かれた期限だけを読み、無ければ出さない
- **寿命を設定で与える (`[credentials.<name>] refresh_lifetime = "90d"` と、起点を login / 最後の refresh から選ぶ `refresh_lifetime_from`)**: provider ごとの寿命の推定 (ローテーションする refresh token の寿命が最初のログインから数えるのか等) を gateway が持つことになり、実態を調べて既定を決める作業と、外れた時の修正が gateway 側に残る。期限はファイルの `expires_at` に人が写し、gateway は読むだけにする (決定 4)。OAuth と static の秘密で仕組みが 1 つになる
- **`credential_expiring` を定期的に繰り返す (1 日 1 回等)**: 間隔に根拠が無く、`expires_at` が変わらない限り同じ事実。1 回にし、しつこく鳴らしたいなら受け手 (bark の `level`) で決める

## 影響

- 転送の知らせの名前が `request` → `llm_request` に変わり、`type` 欄が付く (決定 1)。読み手のうち壊れるのは **`type` の無い件を転送として読む受け口** (ccmsg の `[webhook]` の受け口がこの形。ccmsg は DR-0042 の pull に移れば受け口を持たなくなる) と、SSE の `event: request` を名指しで選んでいるもの。知らない `type` を無視する読み手 (DR-0012) は新しい種類ではそのまま動く
- `llm_request` / `response` の `credential` が経路名から認証情報の名前に変わり、経路名は `route` に移る。`skipped` の要素も `{route, reason}` (決定 2)。`Event` / `Response` / `Skipped` と `Origin` に `route` が増え、`credential` は `Option`。`GET /llm-gateway/sessions` の `routes[]` と欄名が揃う
- `Notice` の全種が `type` を持つので `untagged` の読み分けを `type` で引ける形にできる (読み戻しの順序依存が消える)
- `Notice` に `CredentialRefreshFailed` / `CredentialExpiring` / `CredentialExpired` / `SecretExpiring` / `SecretExpired` が増える (決定 3・4 の 5 種)。SSE と hooks (DR-0040) の両方に流れる
- `credential_refresh_failed` の配線: `refreshing::Inner::record_auth` が前の `AuthStatus` を返し、llm-gateway の `CredentialStore` が差を見て publish する。gateway-core は events を知らないまま
- 認証情報と秘密のファイルのトップに optional の `issued_at` / `expires_at` が増える (決定 4、`StoredCredential` の共通欄)。`issued_at` を gateway が書くのは OAuth の login の書き戻しだけで、refresh の書き戻しと static の置き場には新しい書き込みの経路は増えない。読み戻しは欄が無くても壊れない
- 設定に `[notice]` (`expiry_ratio` / `expiry_before`) が増える。`check` が `ratio` の範囲 (0 < r ≤ 1) を検証する。`daemon reload` で変えられる
- 寿命の判定を 1 時間ごとに回す独立のタスクが増える (`key_rotation::run` とは別)
- MANUAL の events の節に新しい種類 (決定 3・4 の 5 種) と `llm_request` への改名、`route` / `credential` の分け方を足す

## 未決

(無し。kawaz 2026-10-30 に次を裁定: `upstream_sent` は `llm_request` の `ts` (upstream へ送り始めた瞬間、DR-0012) そのものなので足さない。人が読める時刻の欄、`login_path` の絶対 URL 化はいずれも持たない)

## 関連

- [DR-0042](DR-0042-session-keepalive-state-endpoint.md) — 枠の印を pull で読む口 (`routes[] {route, credential?, limited?}`)。`credential_limited` / `recovered` を採らない根拠と、`route` / `credential` の分け方の先行例
- [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) — 本 DR の語彙を購読して、選んだイベントを整形して URL へ配達する `[[hooks]]` (bark 等 push でしか届かない相手が要る時に起こす)
- [DR-0012](DR-0012-request-events.md) — events の契約 (`seq` / `boot`、Unix ミリ秒、未知の `type` は無視、転送の邪魔をしない)
- [DR-0020](DR-0020-denial-reason-visibility.md) — events の語彙を増やす判断の前例。router が組んだ 429 の `"-"` の扱い
- [DR-0030](DR-0030-general-purpose-auth-gateway.md) §2 / §5 — 認証差し替えの中継 (`passthrough`、決定 1 で `llm_request` と名前で分ける理由) と秘密のファイル (決定 4 の `expires_at` / `issued_at` を持たせる先)
- [DR-0023](DR-0023-web-login-endpoint.md) — Web login の口 (`login_path` の指す先、claude_oauth だけ)
- [DR-0010](DR-0010-credential-cross-process-lock.md) — 認証情報ファイルの書き換えと版 (login が `issued_at` を書き戻す経路)
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) §13 — timer のスリープ跨ぎの扱い (決定 4 の周期タスクが同じ構えを取る)
- [DR-0007](DR-0007-usage-visibility.md) — usage の `auth` (`status` / `login_path`)。決定 3 のイベントと同じ値を出す口
- [DR-0032](DR-0032-daemon-reload.md) — reload で差し替えられる欄の区分 (`[notice]`)
