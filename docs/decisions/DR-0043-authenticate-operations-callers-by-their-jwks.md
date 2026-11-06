# DR-0043: 運用系 HTTP の呼び手を、呼び手の JWKS で認証する (ccmsg が最初の呼び手)

- Status: Accepted (kawaz 2026-11-06。決定 1 の `jwks_file` と決定 7 の運用者の鍵は裁定待ち CA-Q5 で、c (daemon が自分の鍵を持ち設定は `jwks_url` だけ) なら書き換える)
- Date: 2026-11-05

## 背景

ccmsg と gateway の連携は ccmsg が取りに来る (pull) だけにする (kawaz 裁定 2026-11-04、DR-0042 背景)。ccmsg は Claude Code の hook のたびに `GET /llm-gateway/sessions/{session_id}` を読み、再起動と webui の初期表示では一覧を読む。ccmsg は tailnet の外にも居るので、Caddy がこの path を外へ通す構成になる。運用系の口 (`/llm-gateway/usage` / `status` / `stats` / `events` / `keepalive/pause`) は手前の境界 (tailnet / Caddy) を信頼して認証を持たない (DR-0006) が、その前提は「tailnet の内側にしか届かない」ことで、外へ通す口には当てられない (DR-0041 決定 6 が JWKS 以外を外に出さない根拠と同じ)。

呼び手を認証する形として、共有 secret (`[webhook] token_file` のような合言葉を両側に配って保管し、漏れたら両側で入れ替える) は持たない。**ccmsg が自分の短命の Ed25519 鍵で各 request を JWT 署名し、gateway は設定に書いた信頼する呼び手の `jwks_url` の一覧で検証する** (kawaz 裁定 2026-11-04)。方式は claude-rules-personal の reference `auth-patterns/webhook-jws-jwks` が正本 (前提 P1〜P4、鍵の 3 状態と 50% / 100% / 110% の遷移、検証の順序、鍵キャッシュと再取得、やらないこと) で、役割は reference と逆: **ccmsg が送り側 (S)、gateway が受け側 (R)**。DR-0041 (hooks の配達を gateway が署名する) は同じ方式を逆向きに当てたもので、鍵も口も別で、互いに依存しない。

本 DR で決めるのは、方式を gateway の受け側に当てた時に決まらないことだけ: 設定の形、署名を求める口、body の無い `GET` に対する token の形 (reference は body を token に内包する webhook 用)、リプレイ対策の形 (短い `exp` を主にし `jti` は任意にする時、`jti` が付いてきたら使用済み集合をどの単位で持つか。stable / unstable は別プロセス)、鍵の取得に使う HTTP、認証を掛ける段階、運用者の CLI と手動確認、ccmsg 側に要るもの。

## 決定

### 1. 設定は `[callers.<name>]` の表。呼び手ごとに `jwks_url` (または `jwks_file`) と `iss` を持ち、`iss` は必ずある

```toml
[callers.ccmsg]
jwks_url = "https://ccmsg.example/.well-known/jwks.json"
iss = "ccmsg"                        # 省略時は表の名前

[callers.operator]
jwks_file = "keys/operator.jwks.jsonl"   # 設定ファイルからの相対。運用者の CLI と curl のための手の鍵束 (決定 7)
```

| 欄 | 意味 |
|---|---|
| `jwks_url` | 呼び手の公開鍵の置き場。https のみ (reference 前提 P1)。`jwks_file` と**どちらか 1 つ**を必ず書く |
| `jwks_file` | 同じホストの呼び手のための鍵束ファイル。ns 認証の `keys_file` (DR-0037) と同じ 1 行 1 JWK (`auth keygen` の出力) で、gateway が読むのは公開部分だけ。相対パスは設定ファイルの置き場から |
| `iss` | token の `iss` と完全一致で比べる名前。省略時は表の名前 (`[callers.<name>]` の `<name>`)。`[callers]` の中で重複しない (`check` が断る) |

**`iss` は必ずある**。署名を求める口は呼び手の数だけあるのではなく、全部の呼び手が同じ口を読むので、reference の「受信 path か `iss` のどちらかで `jwks_url` が一意に引けなければならない」のうち path は使えない。token の `iss` で `[callers.<name>]` を引き、**その呼び手の鍵だけ**から `kid` を探す (reference の「鍵空間を `iss` ごとに分離する」)。別の呼び手の `iss` を名乗った token は、名乗った呼び手の鍵で検証されて落ちる。

表 (`[callers.<name>]`) にするのは、呼び手に自然な名前があり (`[routes.<name>]` / `[credentials.<name>]` / `[ns.<name>]` と同じ)、`extends` (DR-0013) が表は鍵ごとに重ねられるため (配列は丸ごと置換になる)。

`aud` の欄は持たない。`aud` は request ごとに method と path から決まる (決定 3) ので、呼び手ごとの固定値を転記する場面が無い。

### 2. 署名を求めるのは、会話・認証情報・流れを読む口と keepalive を止める口。生存確認・版・自分の状態・ログイン・JWKS は求めない

| 署名を求める | 求めない |
|---|---|
| `GET /llm-gateway/sessions`、`GET /llm-gateway/sessions/{session_id}` (DR-0042)、`GET /llm-gateway/usage`、`GET /llm-gateway/status`、`GET /llm-gateway/stats`、`GET /llm-gateway/events`、`POST /llm-gateway/keepalive/pause` | `GET /llm-gateway/healthz`、`GET /llm-gateway/version`、`GET /llm-gateway/self`、`GET /llm-gateway/login` と `login/{name}/start` / `POST login/{name}`、`GET /llm-gateway/hooks/jwks` (DR-0041) |

求める側は、会話の状態 (sessions)、認証情報の枠と状態 (usage / status)、使った量 (stats)、全会話の流れ (events) を読む口と、会話の keepalive を落とす口 (pause)。外へ通す価値があるのはこれらで、外へ通すなら呼び手を確かめる。

求めない側の理由は口ごとに違う:

- `healthz` / `version` / `self` は**監督者が問う口** (`daemon status` が各 unit の `/self` と `/version` から行を埋め、`restart` が `/healthz` を待つ、DR-0028)。監督者は呼び手の鍵を持たず、持たせると監督者に署名の仕事が増える。`self` が答えるのは「走っているこのプロセス」(版・`boot`・落とした数・動いているダンプ) で、会話も認証情報の状態も無い
- `login*` は**ブラウザで人が操作する口** (DR-0023)。ブラウザは署名できない
- `hooks/jwks` は公開鍵だけで、外へ出してよい唯一の口として DR-0041 が決めてある

求めない口は今までどおり手前の境界を信頼する。**Caddy が外へ通すのは、署名を求める口のうち外の呼び手が読むもの (ccmsg なら `/llm-gateway/sessions*`、使うなら `keepalive/pause`) と `hooks/jwks` だけ**で、求めない口は tailnet の内側に留める (DR-0041 決定 6 の線を、署名で守られた口の分だけ広げる)。

### 3. token は `Authorization: Bearer <JWT>`。claims は `iss` / `aud` (= method + path) / `iat` / `exp` (= `iat` + 10 秒) と任意の `jti` だけで、query と body は覆わない

reference の token は body (`event`) を claims に内包する webhook 用で、本 DR の口は `GET` が主で body が無い。token は本文ではなく **`Authorization` header の Bearer** で運ぶ (RFC 6750)。

```
GET /llm-gateway/sessions/0f3c6d1a-… HTTP/1.1
Authorization: Bearer <compact JWS>

header:  { "alg": "Ed25519", "kid": "<kid>", "typ": "request+jwt" }
claims:  {
  "iss": "ccmsg",
  "aud": "GET /llm-gateway/sessions/0f3c6d1a-…",   # method (大文字) + 空白 1 つ + path。query は含めない
  "iat": 1762300000,                                # 署名時刻
  "exp": 1762300010,                                # iat + 10 秒
  "jti": "<任意。付けるならランダム、request ごとに一意>"
}
```

- `aud` は **method と path** を 1 つの文字列にしたもの。path は gateway が見る path (`/llm-gateway/…`) で、scheme / host は含めない (gateway は自分の外向き URL を知らない、DR-0039 / DR-0041 と同じ理由)。gateway は受けた request の method と path から同じ文字列を組んで完全一致で比べる。Caddy は path を変えずに通す (今の構成のとおり。path を書き換える構成では `aud` が合わず 400 になる)
- **query は含めない**。一覧の `?ns=personal` (DR-0042 決定 2) は答えを絞る条件で、path が同じなら読める範囲は同じ (絞らない方が広い)。query を覆っても守れるものが増えない
- **body は覆わない**。署名を求める口で body を持つのは `POST /llm-gateway/keepalive/pause` (`{"session_id"}`) だけ。書き換えられて起きる最悪は別の会話の keepalive が 1 回落ちることで、次の実リクエストで控えが置き直される (DR-0024 §2)。body まで覆う必要が出た時 (台の挙動を変える口が HTTP に増えた時) は RFC 9421 (HTTP Message Signatures) へ移る (reference の「やらないこと」と同じ判断)
- `typ` は **`request+jwt`**。DR-0041 の `webhook+jwt` と ns 認証の `JWT` とは別の値で、互いに通らない (RFC 8725 の explicit typing)。比べ方は reference のとおり正規化 (小文字化、`application/` 接頭辞を落とす) してから完全一致
- `alg` は **`Ed25519` と `EdDSA` の両方を受ける** (固定、設定にしない)。gateway は ed25519-dalek で直接検証し、鍵は取り込みの時に `kty: OKP` / `crv: Ed25519` を確かめる (決定 5) ので、`EdDSA` の曲線の多義は鍵の側で閉じている。呼び手の JOSE ライブラリが fully-specified の `Ed25519` を出せない場合と、gateway 自身の `auth sign` (`alg: EdDSA`) で手動確認する場合 (決定 7) のため。JWK に `alg` があれば header の `alg` との一致を見る (reference 検証 3)
- **リプレイ対策は短い `exp` を主にする** (reference の「body の無い request への適用」)。この用途の token の寿命 (age) は **10 秒**で、呼び手は `exp` = `iat` + 10 秒で作り、gateway は reference の規則で検証する (枠は `iat` / `exp` の 2 つ、`now` に足し引きしない、`iat` ≤ `now` は見ない)。request ごとに署名し直す (queue に積んで使い回さない) ので、再送でも `exp` 切れにならない
- `jti` は**任意**。付けるなら request ごとに新しくし、gateway は使用済み集合で見る (決定 4 の 7)。無くても断らない。リプレイ防止だけの用で、冪等処理の鍵ではない
- 本 DR の口は読み出しと、副作用が「keepalive を落とす」だけの `keepalive/pause` で、pause も読み出しと同じ扱いにする: `exp` の窓の内側でリプレイされても、keepalive が 1 回余計に落ちる (既に落ちていれば no-op) だけで、次の実リクエストで控えが置き直される (DR-0024 §2)
- `events` (SSE) も同じ形。繋ぐ時の 1 request に署名し、繋がった後の stream は再検証しない (token の `exp` が切れても stream は切らない。切るのは繋ぐ時の判定だけ)。ブラウザの `EventSource` は header を付けられないが、ccmsg は events を繋がない (DR-0042 背景) ので困らない。curl は `-H` で付けられる

### 4. 検証は reference の順序で、`exp` の寿命の上限を状態無しに見る。`jti` が付いてきた時の使用済み集合は unit ごとに持つ

署名を求める口に request が来たら、`[callers]` が空でなければ (決定 6) 次の順で検証する。1 つ落ちたらそこで終わり、救済 (別の呼び手の鍵で試す、`alg` の緩和) はしない:

1. `Authorization` が `Bearer <token>` で、token が compact JWS として parse できる
2. header の `alg` が `Ed25519` / `EdDSA`、`typ` が `request+jwt` (正規化して比較)、`kid` がある。`jku` / `jwk` / `x5u` / `x5c` は無視する
3. claims の `iss` (署名前なのでまだ信じない) で `[callers.<name>]` を引く。無ければ拒否。その呼び手の鍵キャッシュから `kid` を引き、無ければ再取得 (決定 5) して再度引き、それでも無ければ拒否
4. 署名を検証する
5. `now` < `exp` かつ 0 ≤ `exp` − `iat` ≤ 10 秒 (reference の規則、age は 10 秒。差が負の token は 400)
6. `aud` が「method + 空白 + path」と完全一致。`iss` が引いた呼び手の `iss` と完全一致 (3 で引いた以上一致するが、検証後に改めて見る)
7. `jti` があれば使用済み集合で検証し、使用済みなら拒否。無ければ断らない。集合は unit ごとに持ち、`exp` まで保持する (保持の寿命は `exp` の判定と同じ意味なので猶予を足さない。掃除は即時でなくてよく、次の検証のついでか定期でよい)。上限 (65,536 件) に達したら、**使用済みかの照合はして、記録だけ飛ばす** (拒否しない)。満杯で全拒否する方が可用性を削り、照合だけなら満杯中のリプレイも集合に残る分は防げる (実装時に決めた、2026-11-06)

応答は reference の表のとおり **401** (署名不一致、未知 `kid`、`exp` 切れ、`jti` 使用済み、鍵キャッシュが stale の上限超え。呼び手が署名し直せば通りうる) と **400** (`Authorization` の形、parse、`alg` / `typ` / `kid` / `aud` / `iss` の不一致、`iat` / `exp` の欠け、`exp` − `iat` の上限超え。直しても通らない)。401 には `WWW-Authenticate: Bearer error="invalid_token"` (RFC 6750) を付ける。本文は ns 認証と同じ JSON のエラー (`authentication_error`、DR-0008 のとおり英語) で、**`message` は 401 / 400 ごとの固定文にし、どの段で落ちたかは応答に出さず log にだけ残す** (ns 認証と同じ方針: kid の有無や期限切れを外から探らせない。sessions は外へ通す口なのでこの根拠がそのまま当たる。kawaz 2026-11-06)。段の語彙は本 DR 用の `Reason` を新しく持つ (`ns/jwt.rs` の enum には `typ` / `aud` / `iss` / `jti` / stale / `Authorization` 欠落の語が無く、`UnknownKid` が kid 欠落 (400) と未知 kid (401) を兼ねている)。`[callers]` を 1 つも持たない unit は検証せず、`Authorization` があっても見ない。

リプレイの窓は 5 の上限で `exp` の寿命 (10 秒) に閉じ、`jti` はその窓の中をさらに狭める任意の層。**`jti` の使用済み集合は unit ごと (プロセスのメモリ) に持ち、stable / unstable で共有しない** (reference の「`jti` を使う場合、使用済み集合は読み出しの口に限り R のインスタンスごとに持ってよい」)。共有するには request ごとに `[stats] dir` のファイルを flock で読み書きすることになり、読み出しの口の 1 request に書き込みが 1 回増える。守れるのは「同じ token を `exp` の窓の内側で兄弟の unit へ送り直す」リプレイだけで、その結果は読み出しなら同じ答え (控えは共有、DR-0042 決定 2)、pause なら 2 回目は既に落ちている控えに対する no-op。tailnet の内側で token を盗んで窓の内側で兄弟へ送れる相手は、転送系の面 (無認証、DR-0006) を直接叩ける相手でもあるので、ここだけ守っても境界は変わらない。集合はプロセスが落ちれば消え、再起動直後は再起動前の token が `exp` の窓の内側で通りうる (同じ理由で許容)。

### 5. 鍵キャッシュと再取得は reference のとおり。`jwks_url` の取得は専用の小さな HTTP client で https のみ、`jwks_file` は mtime で読み直す

`jwks_url` の呼び手:

- キャッシュの鍵は `jwks_url`、値は JWKS 全体。起動時 (と `daemon reload` で呼び手が増えた時) に全呼び手分を取り、失敗しても起動は続けて最初の request で取り直す
- 再取得に成功したら丸ごと置き換える (載らなくなった `kid` は落ちる)。未知 `kid` での再取得は `jwks_url` ごとに**最小 interval 30 秒**、それと別に**定期 10 分**で取り直す
- 再取得が失敗し続ける間、キャッシュは**最後の成功から 66 分** (鍵寿命 1 時間の 110%、reference の標準値) まで信じ、過ぎたら全拒否 (401)。呼び手の鍵寿命は呼び手の持ち物で gateway は知らないので、reference の標準値を固定で使う
- JWK は `kty: "OKP"`、`crv: "Ed25519"`、`use: "sig"` (あれば) を確かめてから取り込む。`alg` があれば `Ed25519` / `EdDSA` のものだけ取り込む
- 取得に使うのは**この用途の reqwest Client**: https のみ (`http://` は `check` が断る)、redirect を追わない、接続と全体の timeout 5 秒、応答 64 KiB 上限、リトライしない。転送用の Client (`Gateway::new`) は上流の生成を待つために全体の timeout を置かず redirect も追うので、流用しない。proxy / TLS の根は同じ (OS の CA)

`jwks_file` の呼び手:

- ns 認証の手の鍵束と同じ読み方 (`KeyRing`、DR-0037 §16): `current()` のたびに mtime を見て、変わっていれば読み直す。読めない・空・壊れたファイルは採らずに前の束を保ち、同じ mtime の間は警告を繰り返さない。起動時に読めなければ起動を止める (手の鍵束と同じ。設定の誤りを黙って通さない)
- 再取得の interval も stale の上限も無い (ファイルは呼び手でなく運用者が置き、寿命を持たない)。失効は行を消すこと

### 6. `[callers]` が 1 つでもあれば、署名を求める口は署名必須。無ければ今までどおり無認証

`[callers.<name>]` が設定に 1 つでもある unit は、決定 2 の口で `Authorization` の無い request を **401** で断る (無認証の request を通す段階も、通す設定も持たない)。1 つも無い unit は今までどおり認証を持たず、手前の境界を信頼する (DR-0006)。「呼び手を 1 つでも書いた = この unit の運用系の口は呼び手を確かめる」で、設定の有無だけで決まる。

`[callers]` は `daemon reload` で変えられる欄 (DR-0032): 増えた呼び手の鍵を取り、消えた呼び手のキャッシュを捨て、空になれば検証を止める。`check` が断るのは `jwks_url` と `jwks_file` の両方 / どちらも無い、`jwks_url` が https でない (userinfo / fragment を持つ、`http://`)、`jwks_file` が読めない、`iss` の重複、`jwks_url` の重複 (同じ鍵束を 2 つの名前で信頼すると、片方の鍵でもう片方を名乗れる。1 つの呼び手は 1 つの鍵束)、呼び手の名前が `[ns.<name>]` と同じ規則に合わない (英数字と `.` / `_` / `-`)。`jwks_url` へ実際に取りに行くのは `check` ではしない (ネットワークに出ない。起動後の最初の request で分かる)。

### 7. 運用者の CLI と curl は、`jwks_file` の呼び手として署名する

`llm-gateway usage` / `stats` / `upstream status` は稼働中の unit の HTTP の口を読む (DR-0028 決定 6、登録簿から unit の設定を引いて `[server] listen` へ問う)。unit が `[callers]` を持てばこれらも署名が要る。CLI は unit の設定の `[callers.<name>]` のうち **`jwks_file` を持ち、その行に秘密鍵 (`d`) がある呼び手**として署名する: `--caller <name>` で名指し、該当する呼び手が 1 つしか無ければ省略できる。署名できる呼び手が無い unit (`jwks_url` の呼び手しか無い) に対しては、問う前に `caller_required` で断る (401 を貰ってから説明するより、何を書けばよいかが先に分かる)。鍵は `auth keygen` で作り、`[callers.<name>] jwks_file` に書いた鍵束に `>>` で足す (ns の `keys_file` と同じ運用、MANUAL の `auth` の節)。

`daemon status` は署名しない (`/self` / `/version` は求めない口、決定 2)。`dump` は制御 socket で HTTP を通らない (DR-0038)。`login` はブラウザで、求めない口。

鍵束ファイルを読める者は設定ファイルも読める者で、信頼の境界は変わらない (`auth token` が `[stats] dir` の自動の鍵を読んで署名するのと同じ、DR-0037)。鍵が秘密鍵のまま 1 つのファイルに居るのも ns の `keys_file` と同じで、置き場の権限 (600) で守る。

curl での手動確認は、`auth sign` に `--typ request+jwt` と `--aud "GET /llm-gateway/sessions"` を渡して 1 行の token を作り `-H "Authorization: Bearer …"` で付ける形を基本にする (`alg: EdDSA` で出るので決定 3 で受ける)。`auth sign` が `--sub` を必須にしていること、`--typ` を持たないことの扱いは未決 (下記)。

## 採らなかった案

- **共有 secret のまま (`[webhook] token_file` の形を読み出しにも使う)**: 合言葉を両側に配って保管し、漏れたら両側で入れ替える運用が残る。鍵管理ゼロを目的にした reference の方式を既に DR-0041 で採っていて、向きを逆にしても同じ部品で済む (決定 1)
- **mTLS**: CA と証明書の配布が要り、Caddy が TLS を終端する今の構成では gateway にクライアント証明書が届かない (決定 1)
- **gateway の鍵で呼び手を認証する (gateway が発行した token を ccmsg が持つ)**: 向きが逆。token を配る = 共有 secret と同じ運用になり、ccmsg の数だけ発行と失効を gateway が持つ。呼び手が自分の鍵を持ち、公開鍵の置き場だけ gateway に教える形なら、失効は呼び手が JWKS から鍵を外すだけ (決定 1)
- **無認証のまま tailnet の境界に任せる**: ccmsg が tailnet の外に居る構成で Caddy が `/llm-gateway/sessions*` を外へ通すと、全会話の id と状態、認証情報の枠が誰にでも読める。DR-0041 決定 6 が JWKS 以外を外に出さなかった理由がそのまま当てはまる (決定 2・6)
- **`[[callers]]` の配列**: 呼び手には名前があり、`extends` で表は鍵ごとに重ねられる (決定 1)
- **`[callers.<name>]` に `aud` を持つ**: `aud` は request ごとに method と path から決まり、固定値を転記する場面が無い。固定の `aud` にすると 1 つの token で全部の口が読めてしまう (決定 1・3)
- **`aud` に scheme と host も入れる**: gateway は自分の外向き URL を知らない (`Host` header は Caddy の設定次第)。同じ呼び手を信頼する別の gateway へ `exp` の窓の内側で token を回すリプレイは残るが、その 2 台は同じ運用者のもので、決定 4 の兄弟 unit と同じ扱い (決定 3)
- **query も `aud` に含める**: path が同じなら読める範囲は同じで、覆っても守れるものが増えない。含めると query の正規化 (並び、エンコード) を両側で揃える仕事が増える (決定 3)
- **pause の body を claims に内包する (reference の `event` の形)**: 口が 1 つのために token の形を 2 通り持つことになる。書き換えの最悪が「別の会話の keepalive が 1 回落ちる」で、次の実リクエストで戻る (決定 3)
- **body の hash を claims に持つ (`body_sha256`)**: RFC 9421 の部分的な再発明。必要になった時に RFC 9421 へ移る (決定 3)
- **`alg` を `Ed25519` だけにする**: 呼び手の JOSE ライブラリと gateway 自身の `auth sign` が `EdDSA` を出す。曲線は JWK の `crv` で固定しているので、`EdDSA` を受けても多義は無い (決定 3)
- **`jti` 必須 (reference の webhook の形)**: 使用済み集合という状態が要り、保持と掃除が実装の負担になる。怠惰な実装が集合を省くと、`jti` で防ぐはずだったリプレイが丸ごと素通りになる。状態無しで検証できる短い `exp` なら省かれにくい。本 DR の口は読み出しと落とすだけの pause で、`exp` の数秒〜数十秒の窓のリプレイに実害が無い (決定 3・4)
- **`jti` の集合が満杯の時に拒否する (reference の webhook の規則)**: 読み出しの口では `exp` が短く、飛ばしても実害が無い。満杯で全拒否すると呼び手が全部読めなくなり、可用性を削る (決定 4)
- **`jti` の集合を `[stats] dir` で兄弟の unit と共有する**: 読み出しの 1 request に flock 付きの書き込みが 1 回増え、守れるのは tailnet の内側からの `exp` の窓の内側のリプレイだけで、その相手は転送系を直接叩ける (決定 4)
- **`self` にも署名を求める**: 監督者が問う口で、監督者に鍵を持たせることになる。答えはプロセスの状態で、会話も認証情報も無い (決定 2)
- **`healthz` / `version` にも署名を求める**: `restart` の待ちと `daemon status` の行が止まる。生きているかと版は秘密ではない (決定 2)
- **`events` だけ無認証のまま残す**: 全会話の流れを流す口で、sessions より読める範囲が広い。外へ通さないなら tailnet の内側で無認証なのは他の口と同じだが、求める口と求めない口の線は「何を読めるか」で引く (決定 2)
- **転送用の reqwest Client で `jwks_url` を取る**: 全体の timeout が無く redirect を追う設定で、reference の fetch の規則 (timeout 数秒、redirect を追わない、サイズ上限) と合わない (決定 5)
- **署名を求めるかを口ごとの設定にする**: 呼び手の有無で一律に決まる方が、設定の読み手が「この unit の口は守られているか」を 1 か所で判断できる。口ごとの穴を設定で開けられる形にしない (決定 6)
- **`[callers]` を書いても無認証の request を通す移行期間 (`enforce = false`)**: 守っていない期間を設定で作ることになる。移行は「呼び手が署名できるようになってから `[callers]` を書く」の順で足りる (決定 6)
- **CLI の `usage` / `stats` / `upstream status` を制御 socket へ移す**: 同じ答えを HTTP と socket の 2 経路で持つことになり、socket は台の挙動を変える口 (DR-0032) で読み出しの口ではない。署名する方が部品が少ない (決定 7)
- **CLI だけ loopback からの request を無認証で通す**: Caddy も loopback から unit に繋ぐので、送信元では区別できない (決定 7)
- **運用者の鍵を daemon が自動で作って `[stats] dir` に置く (DR-0037 / DR-0041 の形)**: 入れ替えの仕事がもう 1 つ増える。運用者の鍵は同じホストから出ず寿命を要らないので、手の鍵束で足りる (決定 7)

## 影響

- `llm-gateway-server` の決定 2 の口に、`[callers]` が空でない時だけ効く検証の層が入る (`Authorization` の Bearer → 決定 4 の順序)。検証の部品 (compact JWS の parse、ed25519-dalek、JWK の読み込み) は `ns/jwt.rs` から借り、`typ` / `aud` / `jti` の扱いは本 DR の形で別に組む (`JwtAuth` は ns の token 用で `max_ttl` と `sub` を見る)
- `config::Config` に `callers: BTreeMap<String, Caller>` (`jwks_url` / `jwks_file` / `iss`) が増え、`check` が決定 6 の組み合わせを断る。`daemon reload` で差し替えられる
- 呼び手ごとの鍵キャッシュ (`jwks_url` → JWKS、最後の成功時刻、最後の再取得時刻) と、unit ごとの `jti` の集合 (`jti` を付けた token だけ、`exp` を過ぎたら掃く、上限 65,536 で満杯なら照合だけして記録を飛ばす) がプロセスのメモリに増える。取得用の reqwest Client が 1 つ増える
- 401 / 400 の応答と `WWW-Authenticate` が運用系の口に初めて現れる。MANUAL の「Operations」の冒頭 (「None of them carry authentication」) を決定 2 / 6 の形に書き換え、`[callers]` の節と、呼び手が token を作る手順 (claims の形、`auth sign` での手動確認) を足す
- `llm-gateway-cli` の `usage` / `stats` / `upstream status` に `--caller <name>` と署名の経路が増え、署名できる呼び手が無ければ `caller_required` で断る。`auth sign` の扱いは未決
- Caddy の設定: 外へ通す path に、署名を求める口のうち外の呼び手が読むもの (`/llm-gateway/sessions*`、使うなら `/llm-gateway/keepalive/pause`) が加わる。求めない口 (`healthz` / `version` / `self` / `login*`) と、外の呼び手が読まない口は通さない (決定 2)
- **ccmsg 側に要るもの** (設計は ccmsg のリポが持つ): 短命の Ed25519 鍵の 3 状態 (次 / 現 / 旧、鍵寿命 1 時間、50% で次を公開、110% で旧を外す) を回し、`/.well-known/jwks.json` を https で出す (reference の S 側。複数インスタンスなら鍵をファイルに置いて同じ束を出す、前提 P4)。gateway への各 request に決定 3 の token (`iat` は署名時刻、`exp` は `iat` + 10 秒、`jti` は付けてよいが必須でない) を `Authorization: Bearer` で付け、401 / 5xx / 接続失敗は署名し直して送り直す (backoff 付き)、400 は捨てて log。`aud` は request の method と path から組む。時計は OS の時刻同期に任せる (前提 P3)
- **reference `auth-patterns/webhook-jws-jwks` に「body の無い request への適用」の節を足す** (統括が反映): token を `Authorization: Bearer` で運ぶこと、claims から `event` を落とし `aud` を method + path にすること、query / body を覆わない判断と RFC 9421 への移行条件、`typ` を `request+jwt` のように用途ごとに分けること、SSE では繋ぐ時の 1 request だけ検証すること、リプレイ対策を短い `exp` 主体にし `jti` を任意にすること、`jti` の集合をインスタンスごとに持ってよい条件 (読み出しの口で、リプレイの結果が同じ答えか no-op)。本 DR はその節の最初の適用例
- DR-0041 (hooks の署名) とは鍵も口も別で、両方を同時に持ってよい。ccmsg の用途では DR-0040 / 0041 を使わない (DR-0042 背景) ので、gateway の設定に ccmsg 宛ての `[[hooks]]` は書かず ccmsg は受け口を持たない。`jwks_url` を書くのは gateway の設定 (`[callers.ccmsg]`) で、ccmsg が書くのは gateway の根 (DR-0042 決定 6) だけ
- `[webhook]` (`token_file`) は本 DR では触らない。廃止は DR-0040 の提案のまま
- 版が混在する間: `[callers]` を知らない版の unit は設定を `deny_unknown_fields` で断る。stable と unstable が同じ設定を読むなら、両方が新しい版になるまで `[callers]` を書かない (DR-0041 の既知の端と同じ)

### 既知の端

- **呼び手の再起動で鍵が全部変わる構成** (ccmsg が鍵をメモリにしか持たない): 再起動後の最初の request が未知 `kid` で、gateway は再取得して通す。再取得の最小 interval (30 秒) の内側に当たった request は 401 で、ccmsg が署名し直して送り直す。hook のたびの 1 request なので失うものは無い
- **`jwks_url` 側 (ccmsg の https) が落ちている間**: gateway は最後の束を 66 分まで信じ、ccmsg が鍵を回していれば現と事前公開済みの次で検証が続く。66 分を超えると 401 で、ccmsg の https が戻って定期再取得が通るまで読めない。pull の口なので、ccmsg は戻った後の次の hook で読み直せば足りる
- **gateway の再起動**: `jti` の集合が消え、再起動前に通った token が `exp` の窓の内側でもう 1 度通りうる (決定 4 で許容)。鍵キャッシュも消え、起動時に取り直す

## 未決

- **手動確認の道具 (決定 7 の部品)**: `auth sign` を流用するなら `--typ` を足し `--sub` を省略可にする (ns の token の道具に運用系の形が混ざる)。別に `llm-gateway call <METHOD> <path> [--caller <name>]` (署名して問い、答えを出す) を持つなら CLI の命令が 1 つ増えるが、`usage` / `stats` の署名の経路をそのまま使える。どちらにするかは実装の段で決める

(起草時に次を決めた:

- `typ` の値は `request+jwt`。reference に足す節で一般の値として提示し、llm-gateway 固有の名前にしない (決定 3 の部品)
- age は 10 秒 (kawaz 2026-11-05: 10 秒で十分ぬるい。検証の規則は reference に置き、本 DR は値だけ持つ)、`jti` の集合の保持は `exp` まで、上限は 65,536 件 (満杯なら照合だけして記録を飛ばす)。`10 秒 × 受信レート` を十分上回る固定値で、設定にしない (決定 3・4 の部品)
- stale の上限 66 分、未知 `kid` の最小 interval 30 秒、定期 10 分。reference の標準値で固定 (決定 5 の部品)
- 時刻のずれの許容は ns 認証の `SKEW_SECS` (60 秒) を流用せず、本 DR の検証器は猶予を持たない (決定 3 の部品)
- `iat` の未来は見ない (reference の規則)。`nbf` が付いていても見ない (決定 4 の部品)
- `events` の `Access-Control-Allow-Origin: *` はそのまま残す。署名を持たないブラウザは header を付けられないので ACAO は効かないが、外す理由も無い (決定 2 の部品)
- `jwks_file` の相対パスは設定ファイルの置き場から (`keys_file` と同じ) (決定 1 の部品))

## 関連

- claude-rules-personal の reference `auth-patterns/webhook-jws-jwks` — 方式の正本 (前提、鍵の 3 状態、検証の順序、鍵キャッシュ、応答コード、やらないこと)。本 DR は S / R を逆に読み、body の無い request に当てたもの
- [DR-0042](DR-0042-session-keepalive-state-endpoint.md) — 最初の対象の口 (`/llm-gateway/sessions*`) と、ccmsg が pull だけにする裁定
- [DR-0041](DR-0041-hooks-sign-deliveries-with-ephemeral-jwks.md) — 同じ方式の逆向き (gateway が署名し受け口が検証する)。鍵も口も別。Caddy が外へ通す口の線 (決定 6)
- [DR-0037](DR-0037-auto-rotated-ns-jwt-keys-and-auth-token.md) — ns 認証の JWT (`JwtAuth`、`KeyRing` の mtime の規則、`keys_file` の形、`auth keygen` / `sign`)。借りる部品と、`typ` で互いに通らないこと
- [DR-0006](DR-0006-namespace-routing.md) — 運用系の口が手前の境界を信頼していること (本 DR はその前提が外へ通す口には当たらないとして呼び手を確かめる)
- [DR-0028](DR-0028-daemon-service-subcommands.md) — 監督者が `/self` / `/version` / `healthz` を問うこと、CLI が登録簿から unit を引くこと (決定 2・7)
- [DR-0032](DR-0032-daemon-reload.md) — `[callers]` を reload で差し替えること、制御 socket が台の挙動を変える口であること
- [DR-0040](DR-0040-hooks-deliver-events-to-urls.md) — `[webhook]` の廃止の提案 (本 DR は触らない)
- [DR-0023](DR-0023-web-login-endpoint.md) — `login*` がブラウザの口であること (署名を求めない理由)
- [DR-0008](DR-0008-user-facing-language.md) — エラーの文言は英語
