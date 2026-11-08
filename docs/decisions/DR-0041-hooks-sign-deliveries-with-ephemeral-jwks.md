# DR-0041: hooks の配達を gateway のエフェメラル鍵で JWT 署名し、受け口は gateway の JWKS で検証する

- Status: Proposed (ccmsg 連携は DR-0042 の pull に移り、本 DR の用途は bark 等 push でしか届かない相手に限る。その相手が要る時に起こす)
- Date: 2026-10-30

## 背景

`[[hooks]]` (DR-0040) の配達は、受け口への認証を `headers` の `{{secret:<id>}}` (共有 secret の Bearer) でしか表せない。bark のように受け口が固定 token を要求する相手にはそれでよいが、ccmsg のような自前の受け口では、合言葉を gateway と受け口の両方に配って保管し、漏れたら両側で入れ替える運用が残る。kawaz の裁定 (2026-10-30、DR-0040 未決) は、自前の受け口に対しては共有 secret でなく、gateway が短命の鍵で各 POST を JWT 署名し、受け口が gateway の JWKS で検証する方式を採ること。

方式そのものは claude-rules-personal の reference `auth-patterns/webhook-jws-jwks` が正本 (前提 P1〜P4、鍵の 3 状態と 50% / 100% / 110% の遷移、token の形、検証の順序、受け口の鍵キャッシュと再取得、やらないこと)。本 DR はそれを llm-gateway に当てはめた時に決めることだけを書く: 鍵をどこから出すか (DR-0037 の ns 認証の鍵との関係)、stable / unstable の 2 unit (DR-0028) で前提 P4 (JWKS を全インスタンスの鍵で集約) をどう満たすか、JWKS を出す口、`[[hooks]]` に足す欄と署名した時の本文、`{{secret:…}}` との併用、受け口に書く `jwks_url`、前提 P3 (時刻)。

受け口側の実装先は ccmsg が最初で、ccmsg の設計は別リポが持つ。本 DR が ccmsg について書くのは「受け口で JWT を検証する」ことと設定に書く `jwks_url` の形だけ。

## 決定

### 1. 署名の鍵は hooks 専用に持ち、DR-0037 の ns 認証の鍵は使わない

ns 認証 `jwt` の自動の鍵 (DR-0037、`jwt-keys/<ns>.jwks.jsonl`) は流用しない。役目が逆で (ns の鍵は**受け取る** token を検証するための鍵で、hooks の鍵は**送る** token に署名するための鍵)、寿命も違う (ns は `key_rotation` 既定 7 日で token の上限がその周期、hooks は鍵の寿命 1 時間で token は 30 秒)。ns の鍵は `auth = "jwt"` の ns が無ければ存在せず、hooks の署名はどの ns の有無にも依らない。ns の鍵を JWKS で公開すると kid (`<ns>-<日付>-…`) から ns の名前が外に出る。鍵を分ければ、hooks の token が ns 認証に、ns の token が hooks に、互いに通る経路をそもそも作らない (`typ` の検証に加えて鍵でも切れる)。

借りるのは部品だけ: JWK の形 (`private_jwk` / `public_key_text`、Ed25519 の `kty: OKP` / `crv: Ed25519`)、1 行 1 JWK のファイルと脇の `.lock` の flock、追記と一時ファイル + rename、読めないファイルの `.broken.<日時>` への退避、壁時計で次の時刻を計算してタイマーを張る形 (`ns/jwt/auto.rs`)。署名の header は ns 認証の `sign` (`alg: EdDSA`、`typ: JWT`) と別に組む (reference のとおり `alg: Ed25519`、`typ: webhook+jwt`、`kid`)。

### 2. 鍵は `[stats] dir` の `hooks/signing.jwks.jsonl` を兄弟の unit が共有する (前提 P4 の答え)

stable と unstable は同じ面を 2 つの unit で出し、外からは Caddy の `lb_policy first` で 11301 が落ちている時だけ 11302 に回る (DR-0028 決定 4)。受け口が FQDN の `jwks_url` を引いた時にどちらの unit が答えるかは決められないので、**どの unit が答えても同じ JWKS** でなければならない。unit ごとにメモリだけの鍵を持つと、stable が送った token を unstable の JWKS で検証して未知 kid になる。

鍵は `<[stats] dir>/hooks/signing.jwks.jsonl` (脇に `.lock`、ディレクトリ 700、ファイル 600) に 1 行 1 JWK で置き、全 unit が同じファイルから署名し、同じファイルを JWKS として出す。`[stats] dir` は兄弟の unit が共有する走行状態の置き場 (DR-0027 / DR-0035 §6 / DR-0037 §12) で、置き場の設定キーは足さない。`jwt-keys/` の下に置かないのは、`check` が「設定に無い ns の自動の鍵」として挙げる対象 (DR-0037 §13) と混ざるため。

行は `private_jwk` の形に `active_from` (Unix 秒、この鍵で署名してよくなる時刻) と `lifetime` (秒、署名に使う長さ) を足したもの。reference の 3 状態はこの 2 欄と壁時計から決まる:

| 状態 | 区間 | JWKS | 署名 |
|---|---|---|---|
| 次 (pre-published) | `active_from - lifetime/2` 〜 `active_from` | 載る | 使わない |
| 現 (active) | `active_from` 〜 `active_from + lifetime` | 載る | 使う |
| 旧 (retiring) | `active_from + lifetime` 〜 `active_from + 1.1 × lifetime` | 載る | 使わない |

`lifetime` は 1 時間の固定値 (reference の標準値。設定にしない)。寿命の起点は昇格 (`active_from`) で、「次」として置かれている間は寿命を消費しない (reference のとおり)。行に刻むのは、版の違う unit が同じファイルを書いても、消す時刻と昇格の時刻が行だけから決まるようにするため (DR-0037 §11 と同じ理由)。kid は `hooks-<日付>-<16 進 4 桁>` (DR-0037 §9 の形、`<ns>` の位置が `hooks`)。日付は作った unit の地方時。

入れ替えの仕事は DR-0037 §4 / §13 と同じ形: `.lock` を flock で掴む → 読み直す → やることを決める → 書く → 手放す。やることは「現の鍵の `active_from + lifetime/2` が過ぎていて次の鍵が無ければ、`active_from = 現の active_from + lifetime` の鍵を追記する」「`active_from + 1.1 × lifetime` を過ぎた鍵を消す (一時ファイル + rename)」「現の鍵が 1 本も無ければ `active_from = now` の鍵を作る (最初の 1 本、または長く止まっていた後)」。「次」を作る役は lock を掴んだ unit だけで、他の unit はその結果を読む (reference の「各自が作ると束が割れる」) — どちらの unit が先に掴んでも増える鍵は 1 本。次の時刻は、次を作る時刻・昇格の時刻・消す時刻の早いものを壁時計で計算し、寝る長さは「残り」と `lifetime/10` (6 分) の小さい方 (スリープ跨ぎの扱いは DR-0037 §13 と同じ)。

**「現」の選び方は `active_from ≤ now` の行のうち `active_from` が最大のもの**。署名する側 (hook の配達タスク) は、時刻が来るたびに入れ替えの仕事が読み直したメモリの束からそれを取る。その行の `active_from + lifetime` も過ぎていれば「現」は無い (タイマーが遅れた、ファイルが消された) ので、**その場で入れ替えの仕事を 1 回呼んでから**取り直す (flock の内側で「若ければ何もしない」を確かめるので、二重には作らない)。それでも無ければその 1 件は発火せず warning に残す (DR-0040 の失敗の扱い)。

再起動で鍵は作り直さない (ファイルに残っている現の鍵をそのまま使う)。受け口のキャッシュに載っている kid が変わらないので、再起動を跨いでも未知 kid の再取得が起きない。reference の「再起動で全部作り直し」はインスタンスがメモリにしか鍵を持たない場合の話で、ファイルを共有する本決定では成り立たせる必要が無い。

入れ替えの仕事を走らせるのは `sign = true` の hook (決定 4) が設定に 1 つ以上ある unit だけ。署名する者がいなければ入れ替える理由が無い。`daemon reload` で signed な hook が増減すれば、hooks のタスクと一緒に仕事を起こす・止める。

### 3. JWKS は `GET /llm-gateway/hooks/jwks` で出す

```
GET /llm-gateway/hooks/jwks
200 application/json
{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"hooks-2026-10-30-3fa9","alg":"Ed25519","use":"sig","x":"…"}, …]}
```

- 認証なし (`healthz` / `version` と同じ扱い)。載るのは公開鍵だけ
- 決定 2 のファイルの行のうち **`active_from + 1.1 × lifetime` を過ぎていないもの** (次 / 現 / 旧、最大 3 本) を JWK Set (RFC 7517 の `keys`) で出す。ファイルが無い・空・全行が期限切れなら `{"keys":[]}`。出す側で落とすのは、入れ替えの仕事は `sign = true` の hook を持つ unit しか走らせない (決定 2) ので、全 unit から signed な hook が消えるとファイルの掃除も止まり、消されない期限切れの行が JWKS に載り続けるため。読み直しは `KeyRing` と同じ mtime の規則 (DR-0037 §16) で、兄弟の unit が書いた鍵も自分のタイマーを待たずに載る
- JWK の `alg` は token の header と同じ `"Ed25519"` (RFC 9864 の fully-specified)。`auth jwks` が出す ns の JWK (`alg: "EdDSA"`) とは別の関数で組む
- 置き場は `/llm-gateway/` の下 (upstream の API 名と衝突させない、`lib.rs` の方針)。`/.well-known/jwks.json` は持たない (Caddy の後ろに他のサービスも居る root の名前を gateway が取らない)
- stable / unstable のどちらが答えても同じ (決定 2)。Caddy 越しの FQDN で引いた受け口は、どちらの unit が生きていても同じ束を得る

### 4. `[[hooks]]` に `sign` と `aud` を足し、署名した時の本文は `application/jwt`

```toml
[[hooks]]
on = ["llm_request", "response", "cache_expired", "passthrough"]
url = "https://ccmsg.example/webhook/llm-gateway"
sign = true                                   # 既定 false
aud = "https://ccmsg.example/webhook/llm-gateway"   # 任意。既定は url
body = "[{{event}}]"
```

| 欄 | 意味 |
|---|---|
| `sign` | `true` なら本文を JWT にして署名する。既定 `false` (DR-0040 の bark の例はそのまま動く。bark のように固定 token を要求する受け口は署名を検証できないので、書かせない方が自然) |
| `aud` | token の `aud`。既定は `url` に書いたとおりの文字列。受け口が「自分の URL」として持つ値と違う時 (受け口が loopback で聞いていて外向きの名前を自分の URL と思っている等) に上書きする。受け口にはこの値を転記してもらう (reference: 発行するのは送り側) |

`sign = true` の時の本文と header:

- `Content-Type` は `application/jwt`、本文は compact JWS。header は `{"alg":"Ed25519","kid":"<現の鍵の kid>","typ":"webhook+jwt"}`
- claims は reference のとおり `iss` / `aud` / `iat` / `exp` (`iat` + 30 秒) / `jti` (送信試行ごとに乱数) / `event`。`iss` は固定の識別子 `llm-gateway` (URI でなくてよい。gateway は自分の外向き URL を知らないので名前で名指す。受け口は設定に `iss` があれば完全一致で見る)。**`event` には `body` のテンプレートを展開した JSON 値を入れる** (`[{{event}}]` なら 1 件の配列、`{{event}}` なら object)。`body` を書かなければ `{{event}}` とする
- `exp` / `iat` の時刻ずれの猶予 (双方向 30 秒) と `exp - iat` の上限は受け口が持つ (reference の検証 5)。gateway 側は足さない
- **送信試行ごとに署名し直す**。送り直す対象は reference の規則のとおり **401 / 5xx / 接続失敗 (timeout 含む)** で、新しい `jti` と `iat` で署名する (1 回目の `exp` が切れている)。400 系 (401 以外) は直しても通らない (設定か実装の不一致) ので捨てて log に残す。DR-0040 の「4xx は送り直さない」に 401 だけ加わるのは、受け口の 401 が「再署名すれば通りうる」失敗 (未知 kid の再取得の最小 interval、`exp` 切れ、`jti` 集合の満杯) を指すため
- `alg` は fully-specified の `"Ed25519"` (RFC 9864)。受け口のライブラリが `"EdDSA"` しか通さない場合の扱い (`algs` に `EdDSA` を許し、JWK の `crv: "Ed25519"` で曲線を固定する) は受け口側の設定で、reference の token の節が正本。gateway が出す `alg` は変えない
- 受け手の冪等処理の鍵は `event` の中の `seq` / `boot` (DR-0012)。`jti` はリプレイ防止だけ

`check` が断る組み合わせ (設定の誤りを黙って通さない):

- `sign = true` で `body` が JSON 文脈でない (DR-0040 決定 2 の判定で素の文脈になる)。claims に入れる JSON 値が作れない
- `sign = true` で `url` にテンプレート (`{{…}}`) がある。`aud` は受け口が完全一致で比べる固定値なので、発火ごとに変わる URL とは両立しない (`aud` を書いても断る: 送り先が動くのに `aud` が固定なのは設定の矛盾)
- `sign = true` で `headers` に `Content-Type` がある (`application/jwt` に固定)。ヘッダ名の比べ方は大文字小文字を問わない (`content-type` も当たる)
- `sign = true` で `method` が `GET` / `DELETE` (本文を運ばない)
- `aud` があって `sign` が無い (何にも使われない欄を黙って受けない)

`follow_redirects` (同じ origin、最大 3 回、同じ method と本文) は署名しても同じ。`aud` は `url` に書いた値のままで、redirect 先に合わせて変えない (受け口が自分の内側で移すのは受け口の都合)。

### 5. `{{secret:…}}` の Bearer と署名は併用できる

`sign = true` の hook に `headers = { Authorization = "Bearer {{secret:…}}" }` を書いてよい。守る層が違う: 署名は「この本文は gateway が今作ったもの」を受け口のアプリが確かめるためで、header の固定 token は受け口の前に居るもの (認証 proxy、CDN の service token、Basic 認証) を通るため。JWS は本文だけを覆い header を覆わない (reference の「やらないこと」) ので、header の有無は署名の検証に影響しない。ccmsg のように受け口自身が検証するなら header は要らず、書かない。

### 6. 受け口に書く `jwks_url` は Caddy 越しの FQDN (https)

受け口の `jwks_url` は `https://<gateway の FQDN>/llm-gateway/hooks/jwks`。受け口が同じホストでも loopback (`http://127.0.0.1:1130x/…`) にしない:

- reference の受け口は https しか取りに行かない (前提 P1、「fetch は https のみ」)。gateway の都合で受け口側に http の例外を作らない
- loopback は unit のポートを 1 つ選ぶことになる。その unit が落ちている間 (rolling restart、unstable の作り直し) は再取得が失敗する。FQDN なら Caddy がどちらかの生きている unit に回し、決定 2 で両方が同じ束を出す
- 同じホストでも別ホストでも受け口の設定が同じ形になる

Caddy が外 (tailnet の外) に通すのは **`/llm-gateway/hooks/jwks` だけ**。gateway の他の口のうち認証なしのもの (`/llm-gateway/events` / `/usage` / `/self` / `/login/*`) は tailnet の境界を前提に認証を持たない (DR-0006 / DR-0012) ので、`/llm-gateway/*` をまとめて外に出すと会話の流れと認証情報の状態が誰にでも読める。JWKS は公開鍵だけなので外に出してよい唯一の口で、受け口が別ネットワークに居る場合だけこの 1 path を通す。前提 P1 (公開 FQDN + 有効な TLS で `jwks_url` を配信する) は、今の Caddy が公開 FQDN で TLS を終端して gateway に回している構成でそのまま成り立つ。

ccmsg 側の変更は 2 つだけ: 受け口 (`/webhook/llm-gateway`) で `application/jwt` の本文を reference の順序で検証すること、設定に `jwks_url` (と、gateway の `url` が ccmsg の自己 URL と違う時の `aud`) を書くこと。合言葉のファイルは受け口から消える。

前提 P4 (全インスタンスの鍵の集約) は決定 2 + 3 で満たす。**前提 P3 (時刻の同期)** は gateway が何も足さない: `iat` / `exp` は壁時計 (Unix 秒) で、ずれの許容幅は受け口の 30 秒 (reference) のまま。同じホストの ccmsg とは時計が同じで自明、別ホストは OS の時刻同期 (NTP) に任せ、ずれた相手は可用性の問題として扱う (reference P3)。gateway が `check` で確かめられるものではない。

## 採らなかった案

- **DR-0037 の ns の自動の鍵で署名し、その鍵を JWKS で公開する**: 役目 (検証用 / 署名用) も寿命 (7 日 / 1 時間) も違い、`auth = "jwt"` の ns が無いと鍵が無い。公開すると kid から ns の名前が出る。鍵が同じだと `typ` の検証を忘れた受け口で hooks の token が ns 認証に通りうる (決定 1)
- **unit ごとにメモリだけのエフェメラル鍵を持ち、各 unit が自分の JWKS を出す**: 受け口の `jwks_url` が unit を 1 つ指すことになり、Caddy の fail over (どちらが答えるか分からない) の下では stable の token が unstable の JWKS で未知 kid になる。受け口が `iss` で unit を見分けて `jwks_url` を 2 つ持つ形は、同じホストの loopback でしか成り立たない (FQDN は unit を選べない)。秘密鍵をディスクに置かない利点はあるが、前提 P4 が崩れる (決定 2)
- **unit ごとの鍵をメモリに持ち、公開鍵だけを `[stats] dir` に書いて各 unit の JWKS が全 unit の公開鍵を合わせて出す**: P4 は満たせるが、落ちた unit の公開鍵の掃除、unit の名前でのファイル分け、版の違う unit の書式の揃え、と新しい仕組みが要る。DR-0037 の flock + 1 ファイルを借りれば同じ結果がもっと少ない部品で出る。秘密鍵が state に置かれる危険は DR-0037 の自動の鍵と同じ種類で、1 回の写しなら寿命 1 時間の鍵は最長 1.6 時間で効かなくなる (決定 2)
- **監督者 (`daemon supervise`) が JWKS を集約して出す**: 監督者は HTTP を持たない (DR-0028 決定 3)。集約のために口を足すと監督者が面を持つことになる
- **`sign` の既定を `true` にする**: DR-0040 の bark の例が全部 `sign = false` を書くことになり、固定 token しか受けない受け口に `application/jwt` を送って壊れる。署名を検証できる受け口は自前のものだけで、そちらが opt-in する方が書く量が少ない (決定 4)
- **`/.well-known/jwks.json` に置く**: root の名前は Caddy の後ろの他のサービスと競合する。gateway 自身の口は `/llm-gateway/` の下に揃っている (決定 3)
- **受け口が同じホストなら `jwks_url` を loopback にする**: 受け口の https 限定に例外を作り、unit を 1 つ選ぶことで rolling restart の間の再取得が落ちる (決定 6)
- **鍵の寿命を設定で持つ**: 変える理由が無く (reference の成立条件 `10% × 鍵寿命 > 30s + 30s + 受け口の再取得 interval` は 1 時間で余裕がある)、短くすると成立条件を破る口になる。DR-0040 の queue の長さと同じく固定 (決定 2)
- **detached JWS (`b64: false`) で本文を平文 JSON に保つ**: reference の「やらないこと」。受け口が検証後に claims をログに出せば足りる

## 影響

- `hooks.rs` (DR-0040) に署名の経路が増える: 現の鍵で header `{alg: Ed25519, kid, typ: webhook+jwt}` と claims `{iss, aud, iat, exp, jti, event}` を組み、`application/jwt` で送る。送り直しのたびに署名し直し、`sign = true` の hook では 401 も送り直す対象に入る
- `[stats] dir` の下に `hooks/signing.jwks.jsonl` と `.lock` が増える (`sign = true` の hook がある unit だけが作る)。置き場の導出は `Config::auto_keys_path` と同じく 1 つの関数に置き、daemon と `check` が使う。`.broken.<日時>` への退避と一時ファイルの掃除は `ns/jwt/auto.rs` の形を借りる (印の欄が `active_from` / `lifetime` になるので、`plan` は別に書く)
- `GET /llm-gateway/hooks/jwks` が増える (認証なし)。受け口が tailnet の外に居るなら Caddy で **この 1 path だけ**を外に通す (決定 6)。`/llm-gateway/*` をまとめて外に出さない
- `config::Hook` に `sign` / `aud` が増え、`check` が決定 4 の組み合わせを断る。どちらも `daemon reload` で変えられる欄 (hooks のタスクを組み直す時に入れ替えの仕事も起こす・止める)
- DR-0040 決定 3 の ccmsg の受け口の書き方は、`Authorization = "Bearer {{secret:ccmsg-webhook}}"` の無い `sign = true` の形になる。`[secret_store]` の `ccmsg-webhook.json` は要らなくなる (bark の `{{secret:bark}}` はそのまま)
- ccmsg は受け口で `application/jwt` を検証し、設定に `jwks_url` を持つ。合言葉の照合は消える。MANUAL の hooks の節に `sign` / `aud` と JWKS の口を足す
- 秘密鍵が state に 1 つ増える。漏れると受け口に gateway を騙った event を送れる。鍵の寿命が 1 時間なので **1 回の写しなら**最長 1.6 時間で効かなくなるが、ファイルを読み続けられる状態 (置き場への読み取りが残っている) では次の鍵も読まれるのでこの上限は当てはまらない。守りは置き場の権限 (700 / 600) で、漏れが分かったら読み取り経路を塞いでからファイルを消す (消せば次の周期で新しい束になる)

### 既知の端

- **全 unit が長く止まっていた後の再開**: 次の鍵が事前公開されないまま現の鍵の期限の直前に再開すると、昇格までの間が reference の成立条件 (90 秒) を切ることがある。昇格で受け口が未知 kid を見て再取得するので、再取得の最小 interval の内側に当たった件は 401 で拒否される。401 は 1 回だけ送り直す (決定 4) が、送り直しも interval の内側なら再び 401 で落ちる。停止中の件はどのみち配達されていないので、新しく失うのはこの数件だけ
- **版が混在する間**: `hooks/signing.jwks.jsonl` を知らない版の unit は署名しないだけで、ファイルを読みも書きもしない。新しい版だけが `sign = true` の hook を持てる (古い版は `sign` を `deny_unknown_fields` で断る)。stable と unstable が同じ設定を読むなら、両方が新しい版になるまで `sign` を書かない
- **Caddy が落ちている間**: 配達 (gateway → 受け口) は Caddy を通らないので続くが、受け口の再取得 (`jwks_url`、決定 6) は失敗し、受け口はキャッシュの束を使い続ける。現の鍵と事前公開済みの次の鍵で最長 1.5 時間は検証が続き、それを超えると Caddy が戻るまで未知 kid で拒否される

## 未決

(無し)

## 関連

- claude-rules-personal の reference `auth-patterns/webhook-jws-jwks` — 方式の正本 (前提、鍵の 3 状態、token の形、検証の順序、受け口のキャッシュ、やらないこと)。本 DR はこれに llm-gateway の構成を当てはめたもの
- [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) — `[[hooks]]` の配達 (本 DR が `sign` / `aud` を足す先)。失敗の扱い、送り直し、`{{secret:<id>}}`
- [DR-0039](DR-0039-credential-events-and-llm-request.md) — 流れる events の語彙 (`event` claim の中身)、gateway が自分の外向き URL を知らないこと (`login_path`)
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) — ns 認証の自動の鍵。借りる部品 (flock、追記 / rename、退避、タイマー) と、流用しない理由 (決定 1)
- [DR-0028](DR-0028-daemon-service-subcommands.md) — stable / unstable の 2 unit と Caddy の fail over (前提 P4 の出どころ)、監督者は HTTP を持たない
- [DR-0027](DR-0027-keepalive-by-replay.md) / [DR-0035](DR-0035-judge-thinking-crossing-by-session-sources.md) §6 — `[stats] dir` を兄弟の unit が共有する先例
- [DR-0032](DR-0032-daemon-reload.md) — reload で差し替えられる欄の区分 (`sign` / `aud`)
- [DR-0012](DR-0012-request-events.md) — events の `seq` / `boot` (受け手の冪等処理の鍵)
- [DR-0006](DR-0006-namespace-routing.md) — 認証なしの口が tailnet の境界を前提にしていること (決定 6 で JWKS 以外を外に出さない根拠)
