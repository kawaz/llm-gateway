# ns 認証 `jwt` と helper CLI の実装計画 (DR-0030 §7 手順 4)

- Status: 計画 (未裁定)。裁定が要る点は末尾「裁定が要る点」にまとめる
- 前提: DR-0030 §5 / §6 (裁定済み)、DR-0031 §2 (1)、DR-0013、DR-0012、DR-0029、DR-0028、DR-0008
- 出発点: `crates/gateway-core/src/ns.rs` の `NsAuth` (固定 token だけ、`#[serde(transparent)]` の `Option<String>`)、`Principal { ns, subject, kid }`、`Authorization { Accepted, WrongToken, Open }`。`crates/llm-gateway/src/config.rs` の `Namespace.auth` は `rename = "auth_token"`

## 1. 設定の形

### 例

```toml
# 固定 token (現行と同じ書き方。これはこのまま動く)
[ns.personal]
auth_token = "十分に長い乱数"

# jwt
[ns.claude]
auth = "jwt"
max_ttl = "400d"          # 必須。これより長い寿命 (exp - iat) の token は受けない
iss = "llm-gateway-cli"   # 任意。書いたら一致を要求
aud = "ns-claude"         # 任意。書いたら aud (文字列 or 配列) に含まれることを要求

[[ns.claude.keys]]
kid = "claude-2026-09"
alg = "EdDSA"             # 省略時 EdDSA。今は EdDSA 以外を書くと設定エラー
public = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"   # Ed25519 公開鍵 32 byte の base64url (パディング無し)

[[ns.claude.keys]]
kid = "claude-2026-12"    # ローテ中は新旧 2 本が並ぶ
alg = "EdDSA"
public = "..."
```

### 決め方

- **`auth` は方式名の文字列 1 つ + 方式ごとの欄を ns 直下に並べる形**を推す。`auth = { jwt = { keys = [...] } }` の入れ子 (externally tagged enum) は TOML で `[[ns.claude.auth.jwt.keys]]` と深くなり、手で書く・`extends` で差し替える単位が見えにくい。serde では `Namespace` に生の欄 (`auth: Option<AuthKind>`、`auth_token`、`keys`、`max_ttl`、`iss`、`aud`) を受け、読み込み後の検証で `NsAuth` enum に畳む (`#[serde(try_from = "RawNsAuth")]` 相当)。方式と欄の食い違い (`auth = "jwt"` なのに `keys` が空、`auth = "token"` なのに `keys` がある 等) は設定エラーにする
- **現行互換**: `auth` を書かず `auth_token` だけ書いたら `token` 方式。`auth` も `auth_token` も無ければ今どおり無検査 (DR-0006)。`auth = "token"` と明示してもよい (その時は `auth_token` 必須)。既存設定は 1 行も変えずに動く
- **`NsAuth` の中身**を enum にする:

  ```rust
  pub enum NsAuth {
      Open,
      Token(String),
      Jwt(JwtAuth),   // keys: BTreeMap<kid, VerifyingKey>, max_ttl, iss, aud
  }
  ```

  `transparent` をやめるので `NsAuth::token()` / `is_open()` / `verify()` の外形は保ち、`Serialize` は上の生の欄へ戻す形で書く (`check` 等で設定を書き出す経路があれば崩さないため)
- **公開鍵の表記**は生の 32 byte の base64url を第一とする。JWK (`{"kty":"OKP","crv":"Ed25519","x":"..."}`) の `x` と同じ値なので、helper CLI の JWKS 断片からそのまま転記できる。JWK オブジェクトを丸ごと書ける形 (`jwk = {...}`) は、外部の IdP の JWKS を貼りたい需要が出た時に足す (今は 1 表記に絞って読み違いを減らす)
- **鍵を設定の外のファイル (JWKS ファイル) に置く案**は §6 リスクで比較する。推しは「第一版は設定内」

### `extends` (DR-0013) との相性

DR-0013 は「表は再帰マージ、配列は丸ごと置換」。`keys` を配列 (`[[ns.x.keys]]`) にすると派生側で 1 本足すつもりが土台の鍵を全部消す。逆に言えば「派生側に書いた鍵だけが有効」で、失効 (kid 削除) を派生側で確実に表せる利点もある。

代案は **kid をキーにした表** (`[ns.claude.keys.claude-2026-09] public = "..."`)。これだと表のマージになり、派生側で 1 本足せるが、土台の鍵を派生側で消す書き方が無くなる (DR-0013 に削除記法が無い限り)。失効が kid 削除で表される以上、「消せない」は致命的なので **配列を推す**。runbook に「`keys` は土台から丸ごと書き写す」と書く。

## 2. 検証の実装

### crate

| 候補 | ヘッダの alg を信用しない | 依存 | 評価 |
|---|---|---|---|
| `jsonwebtoken` | `Validation::new(Algorithm::EdDSA)` で alg を固定でき、ヘッダの alg が違えば弾く。ただし「kid → 鍵」の引き当てとヘッダ解析の順序は呼び出し側で書く | `ring` (または版により `aws-lc-rs`)、`simple_asn1`、`pem` 等。既存 workspace に `ring` は無い | 動くが重い。Ed25519 だけのために汎用 JOSE を入れる |
| `ed25519-dalek` + 自前の JWS compact 解析 | 自前なのでヘッダの `alg` は「kid ごとの設定値と一致するか」の照合にしか使わない形を構造で保証できる | `ed25519-dalek` (+ `curve25519-dalek`、`sha2`)。`base64` / `serde_json` は既存 | **推し**。JWS compact は `b64(header).b64(payload).b64(sig)` の分割と、署名入力 = 先頭 2 段のバイト列だけで、書く量は 100 行程度 |
| `jwt-simple` | 鍵型が alg を決めるので安全側 | `ed25519-compact` 等、中程度 | 候補にはなるが API が大きく、`issued` で発行側も書く時に自前の方が揃う |

推しは `ed25519-dalek` + 自前。理由: DR-0030 §6 の「alg は kid ごとに設定で固定」は、ライブラリの `Validation` に任せるより **検証関数の引数に alg を持たせず、kid から引いた設定値の型 (`VerifyingKey`) が alg そのもの**という形の方が崩れない。`issued` (手順 5) で署名側を書く時も同じ crate で済む。`verify_strict` を使い、小位数点・非正準な署名を弾く。

置き場: 検証ロジックは `gateway-core::ns` (または `gateway-core::ns::jwt`)。LLM の語彙を含まないので汎用層に置ける。`ed25519-dalek` は `gateway-core` の依存になる。

### 検証の順序

1. `Bearer ` を剥がす (現行どおり `Bearer` 無しも受けるかは下記)。`.` で 3 段に割れなければ拒否
2. ヘッダを base64url 復号 → JSON。`kid` が無い / 設定に無い → 拒否。`alg` が kid の設定値 (`EdDSA`) と一致しない → 拒否 (`none` はここで落ちる)。`crit` があれば拒否 (理解できない拡張を黙って無視しない)。`typ` は見ない (付けない実装が多い)
3. 署名を検証 (署名入力は受け取った先頭 2 段の生バイト列。再エンコードしない)
4. 署名が通ってから payload を JSON として読み、claim を検査

### claim

| claim | 扱い |
|---|---|
| `exp` | 必須。`now > exp + skew` で拒否 |
| `iat` | **必須を推す**。`max_ttl` の判定 (`exp - iat <= max_ttl`) に要る。`iat > now + skew` (未来発行) も拒否 |
| `nbf` | あれば `now + skew < nbf` で拒否 |
| `sub` | 必須。空文字列は拒否。`Principal.subject` に入れる |
| `iss` / `aud` | ns 設定に書いた時だけ照合。書いていなければ値があっても見ない |
| `jti` | 見ない (`issued` の refresh 再利用検知で使う時に足す) |
| その他 | 無視 |

- **寿命上限**: `exp - iat` で測る。`iat` を偽って短く見せても署名者 = 鍵保持者なので、上限は「鍵保持者の誤りを弾く」歯止めであって攻撃対策ではない。加えて `exp - now <= max_ttl` も見る (iat を過去に置いて exp を遠くする形を弾く)
- **時計の揺れ**: skew は固定 60 秒を推す (設定欄にしない。ns ごとに変えたい場面が見えない)
- **`sub` の文字種**: events / stats のキーになるので、長さ上限 (例 128) と制御文字の拒否を入れる

### 失敗時の応答

- 状態は 401。本文と `WWW-Authenticate: Bearer error="invalid_token"` だけで、**どの検査で落ちたかは応答に出さない** (kid の存在・期限切れ・署名不一致を外から区別させない)
- 理由は gateway 内の観測に残す。`Authorization` に `Rejected(Reason)` を足し (`WrongToken` は `Rejected(Reason::Mismatch)` 相当に畳むか並べるかは実装時に決める)、理由は `unknown_kid` / `bad_signature` / `expired` / `ttl_exceeded` / `claims` / `malformed` 程度の粗さ。これを tracing のログと events (§4) に出す。**`exp` 切れだけは応答で区別したい需要がある** (Claude Code の利用者が「期限切れ」と分かれば runbook に辿れる) — 裁定点
- 現行の `rejection(ns, ns_name, headers)` (名乗り方の違いを案内する経路) は token 方式のもの。jwt 方式でも同じ案内を出すかは実装時に読む

### 名乗り方

Claude Code は `ANTHROPIC_AUTH_TOKEN` を `Authorization: Bearer` で送る。LLM 経路で `x-api-key` を受けている箇所があるなら、jwt 方式でもそこから取るかを揃える (現行の token 方式の受け口と同じ集合にする)。

## 3. helper CLI

### サブコマンド

DR-0028 の体系 (`daemon` / `service` / `upstream` は子を持つレベル) に合わせ、子を持つ `auth` を足す:

```
llm-gateway auth                     # help を出す
llm-gateway auth keygen [--kid <kid>]
llm-gateway auth jwks --key <file>
llm-gateway auth sign --key <file> --sub <subject> --ttl <dur> [--iss <iss>] [--aud <aud>]...
```

- `keygen`: Ed25519 鍵ペアを作り、**秘密鍵を標準出力**に出す。形式は JWK (`{"kty":"OKP","crv":"Ed25519","kid":"...","d":"...","x":"..."}`) を推す (kid と公開鍵を同梱でき、`jwks` / `sign` が 1 ファイルで完結する)。`--kid` 省略時は日付 + 乱数 (例 `2026-09-24-3f9a`)。ファイルへの直接書き出し (`--out`) は持たない — 利用者が `> key.jwk` し `chmod 600` する。持たせると「CLI が秘密をどこかに置いた」になり §5 の線がぼやける。ただし umask 次第で 644 で残る懸念があり、裁定点に挙げる
- `jwks --key <file>`: 秘密鍵ファイルから **設定に貼る TOML 断片** (`[[ns.<name>.keys]]` の kid / alg / public) を出す。`--format jwk` で JWKS JSON (`{"keys":[...]}`) も出せる。ns 名は `--ns <name>` で埋める (省略時はプレースホルダ)
- `sign`: 秘密鍵で JWT を鋳造して標準出力に 1 行。`iat = now`、`exp = now + ttl`。`--ttl` は `max_ttl` と同じ書式 (`90d` 等)。設定ファイルは読まない (`max_ttl` との照合は gateway 側の責務。CLI が照合すると設定の場所が要り、手元に設定が無い鋳造の妨げになる)。`--key -` で標準入力から読めるようにし、秘密鍵をファイルに落とさず 1Password から流す運用 (`op read ... | llm-gateway auth sign --key - ...`) を可能にする
- `issued` の bootstrap 発行 (DR-0030 §6) は手順 5 で `auth issue` 等として足す。gateway の署名鍵ファイルを読む点で `sign` とは責務が違うので、`sign` に混ぜない
- help はテキスト (DR-0028 §8)、文言は英語 (DR-0008)。引数なしで help、ロングオプションのみ、`--` セパレータ対応、completion 定義を同時に追従 (cli-design-preferences)。completion を今この CLI が持っているかは実装時に確認し、持っていれば `auth` を足す

### Claude Code 向けの運用 (runbook の骨子)

鋳造:

1. `llm-gateway auth keygen --kid claude-2026-09 > claude-2026-09.jwk` (秘密鍵は 1Password の個人 vault に保存し、ファイルは消す)
2. `llm-gateway auth jwks --key claude-2026-09.jwk --ns claude` の出力を設定の `[ns.claude]` に貼り、daemon を rolling restart (`daemon restart --all`)
3. `llm-gateway auth sign --key claude-2026-09.jwk --sub kawaz-mbp --ttl 180d` の出力を、各機械の Claude Code `settings.json` の `env.ANTHROPIC_AUTH_TOKEN` に貼る。`sub` は機械 (または用途) ごとに分けると events で見分けられる

ローテ (周期は裁定点。推しは鍵 180 日、token の ttl は鍵周期 + 猶予):

1. 新しい鍵を keygen、断片を `keys` に**追加** (旧 kid は残す) → restart
2. 新 kid で各機械の token を鋳造し直して貼り替え。走行中の Claude セッションは起動時の env を持ち続けるので、全セッションの再起動を待つ
3. events で旧 kid の利用が 0 になったのを確かめて旧 kid を `keys` から削除 → restart。これが失効

漏洩時: 該当 kid を即削除 → restart (その kid の token は全て即失効)。別 kid で鋳造した他の機械は影響を受けない — これが「機械ごとに kid を分ける」案の利点で、kid の粒度 (1 ns 1 鍵か、機械ごとか) は裁定点。

## 4. Principal の下流

- **events**: `request` (DR-0012) と `passthrough` に `subject` と `kid` を足す。どちらも `skip_serializing_if = "Option::is_none"` で、token 方式・無検査の ns では欄自体が出ない (既存の読み手を壊さない。DR-0012 の「欄自体は必ず出す」は `origin` の規定で、新欄を常時出す義務ではない)。`request` と対になる完了の知らせ (DR-0012 の 2 通目) にも同じ値を載せる (DR-0012 の「見る側に 2 通の突き合わせを強いない」)
- **認証失敗**: 今は 401 は events に出ていない (passthrough は `refused` 付きで出るが認証失敗の前段で返っている)。jwt の拒否理由を観測するため、`passthrough` に `refused = "auth"` + `auth_error = "<reason>"` を出すか、ログだけにするかは裁定点。推しはログだけ (未認証の相手の入力で events の流量が増える口を作らない)
- **stats**: 第一版では**足さない**を推す。DR-0029 で origin 軸を足したばかりで、軸を増やすと保存形式と CLI の `--by` が増える。subject ごとの使用量が要るなら `--by subject` を DR-0029 と同じ形 (既定は出さず、問い合わせで開く) で後から足す。events に subject が載れば外部集計はできる
- **レート制限 / allowlist**: 今は ns 単位。subject 単位の枠は本手順の範囲外 (Principal に揃えたので後から足せる、が境界)

## 5. 段分け

Phase 0 の 3 行 (完了条件):

- 目的: ns の `auth` に `jwt` 方式を足し、helper CLI で鋳造した Ed25519 JWT で Claude Code が ns を通れるようにする
- 完了の判定: `auth = "jwt"` の ns に対し、`auth sign` で作った token が通り、期限切れ・未知 kid・alg 違い・署名改竄・寿命超過が 401 になる e2e テストが `just ci` で緑。既存の `auth_token` 設定は無変更で通る
- 範囲外: `issued` (発行の HTTP endpoint、bootstrap、署名鍵の保存)、JWKS の Persistence 化、設定の稼働中再読込、subject 単位の stats / 枠

段 (各段 `just ci` 緑で閉じる):

1. **`NsAuth` を enum 化** (振る舞い不変)。`Open` / `Token` の 2 枝、生の欄からの畳み込み、方式と欄の食い違いの設定エラー。既存テスト全部そのまま緑 + 食い違いのテスト
2. **jwt 検証** (`gateway-core`)。`ed25519-dalek` 導入、JWS 解析、claim 検査、`Rejected(Reason)`。単体テストで上の拒否 5 種 + `none` + `crit` + 未来 `iat` + skew 境界。テスト用の鍵はテスト内で生成
3. **設定と経路の接続**。`auth = "jwt"` / `keys` / `max_ttl` / `iss` / `aud` の読み込み、LLM 経路と passthrough の両方で jwt を通す、401 + `WWW-Authenticate`、`extends` で `keys` が置換されるテスト。e2e で Claude 形式のリクエストが通る
4. **Principal を events へ**。`request` / 完了 / `passthrough` に `subject` / `kid`。無い時は欄が出ないことをテストで縛る
5. **helper CLI** `auth keygen` / `jwks` / `sign`、help、completion。`sign` の出力を段 2 の検証に通す往復テスト (CLI と検証器が同じ規約で合っていることの証明)
6. **docs**: README / 設定例、runbook (`docs/runbook/` 等、既存の置き場に合わせる)、DR-0030 の Status に手順 4 実装済みを記す

`issued` との境界: 手順 4 の検証器は「kid → 公開鍵」の表を受けるだけにし、表の出どころ (設定 or gateway 自身の JWKS) を知らない。手順 5 は同じ検証器に gateway の JWKS から作った表を渡し、発行側 (署名、kid ごとの最終発行時刻、token endpoint) を足す。JWKS を DR-0031 (1) の Persistence に載せるのは `issued` の署名鍵 (gateway 自身の秘密) で、`jwt` の公開鍵は秘密ではないので設定内でよい、という線を引く。

## 6. リスク・未確認

- **長寿命 JWT の漏洩窓**: 失効手段は kid 削除だけなので、漏れた token は kid を消すまで ns の全権限で通る。token 単位の失効 (jti の拒否リスト) は持たない。緩和は kid の粒度を細かくする (機械ごと) ことと、ns の allowlist / 枠。漏れた token は `settings.json` (平文) から漏れるのが典型で、これは現行の固定 token と同じ危険度。jwt で良くなるのは「どの機械の token か (sub / kid) が events で分かる」「1 台分だけ失効できる」点
- **JWKS の置き場**: 設定内 (推し) は、公開鍵は秘密でないので設定と一緒に管理でき、restart で反映が揃う。欠点は設定ファイルが長くなることと、鍵の差し替えに設定の編集が要ること。ファイル参照 (`keys_file = "..."`) は外部 IdP の JWKS を置く時に便利だが、読込時点と反映の規則を別に決める必要がある。第一版は設定内だけにし、需要が出たら足す
- **`extends` の配列置換**: §1 のとおり、派生側で `keys` を書くと土台の鍵は全部消える。意図どおり (失効を確実に表せる) だが、1 本足すつもりで旧鍵を消す事故がありうる。`check` サブコマンドで「土台と派生で keys が違う ns」を表示する程度の手当てを検討
- **kid 削除の反映タイミング**: 現状、設定の稼働中再読込は無い (DR-0028 の `daemon restart` で読み直す、と読んだが、SIGHUP 等の再読込が本当に無いかは未確認)。失効 = restart で、rolling restart 中は旧 kid が片側の unit で数秒生きる。漏洩時の失効としては許容範囲と見るが、裁定点
- **時計**: gateway 機の時計が大きくずれると全 token が一斉に失効 / 通過する。skew 60 秒を超えるずれは NTP 前提で扱わない
- **`ed25519-dalek` の版**: 2 系の `verify_strict` を前提にしている。workspace の他依存 (`sha2` 等) との版衝突は未確認
- **Claude Code の挙動**: `ANTHROPIC_AUTH_TOKEN` に 300 byte 程度の JWT を入れて問題が無いかは未確認 (長さ制限は無いと見ているが、実機で 1 回確かめる)。`ANTHROPIC_AUTH_TOKEN` があるとサブスクとしての振る舞いをやめる件 (config.rs のコメント) は token 方式と同じで、jwt 方式で変わらない

## 裁定が要る点

### 対外仕様 (設定・CLI・応答の形) に触るもの

1. 設定の形: `auth = "jwt"` + ns 直下に `keys` / `max_ttl` / `iss` / `aud` を並べる (推し) か、`auth = { jwt = {...} }` の入れ子か
2. `keys` を配列にする (推し、`extends` で丸ごと置換) か kid をキーにした表にするか
3. 公開鍵の表記を base64url 32 byte だけにする (推し) か JWK も受けるか
4. `iat` と `sub` を必須にするか (推し: 両方必須)
5. 401 で期限切れだけ区別して返すか (推し: 区別しない、ただし利用者の導線は runbook で補う — 反対意見あり得る)
6. CLI の名前と形: `auth keygen` / `auth jwks` / `auth sign`、秘密鍵は JWK を標準出力 (ファイル書き出しオプションを持たない) で良いか
7. events に `subject` / `kid` を足す (推し) 範囲と、認証失敗を events に出すか (推し: 出さない)

### 運用・内部

8. Ed25519 の実装に `ed25519-dalek` + 自前 JWS を使う (推し) か `jsonwebtoken` か
9. kid の粒度 (ns に 1 本か、機械ごとか) とローテ周期 (推し: 機械ごと、鍵 180 日)
10. 失効の反映を restart に頼ってよいか (稼働中再読込を別途作るか)
11. stats に subject 軸を足さない (推し) で良いか
