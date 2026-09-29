# ns 認証 `jwt` と helper CLI の実装計画 (DR-0030 §7 手順 4)

- Status: 計画 (裁定済み。末尾「裁定済み」)
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
keys_file = "~/.config/llm-gateway/keys/claude.jwks.jsonl"   # 必須。1 行 1 JWK の鍵束
max_ttl = "400d"          # 必須。これより長い寿命 (exp - now、iat があれば exp - iat も) の token は受けない
iss = "llm-gateway-cli"   # 任意。書いたら一致を要求
aud = "ns-claude"         # 任意。書いたら aud (文字列 or 配列) に含まれることを要求
```

鍵束 `claude.jwks.jsonl` (600。各行は `auth keygen` の出力そのもの、ローテ中は新旧 2 行が並ぶ):

```
{"kty":"OKP","crv":"Ed25519","kid":"claude-mbp-2026-09","d":"…","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}
{"kty":"OKP","crv":"Ed25519","kid":"claude-mbp-2027-03","d":"…","x":"…"}
```

### 決め方

- **`auth` は方式名の文字列 1 つ + 方式ごとの欄を ns 直下に並べる**。`auth = { jwt = { ... } }` の入れ子 (externally tagged enum) は TOML で深くなり、手で書く・`extends` で差し替える単位が見えにくい。serde では `Namespace` に生の欄 (`auth: Option<AuthKind>`、`auth_token`、`keys_file`、`max_ttl`、`iss`、`aud`) を受け、読み込み後の検証で `NsAuth` enum に畳む。方式と欄の食い違い (`auth = "jwt"` なのに `keys_file` が無い、`auth = "token"` なのに `keys_file` がある 等) は設定エラーにする
- **現行互換**: `auth` を書かず `auth_token` だけ書いたら `token` 方式。`auth` も `auth_token` も無ければ今どおり無検査 (DR-0006)。`auth = "token"` と明示してもよい (その時は `auth_token` 必須)
- **`NsAuth` の中身**を enum にする:

  ```rust
  pub enum NsAuth {
      Open,
      Token(String),
      Jwt(JwtAuth),   // 鍵束 (kid → 鍵), max_ttl, iss, aud
  }
  ```

  `NsAuth::token()` / `is_open()` / `verify()` の外形は保ち、`Serialize` は上の生の欄へ戻す形で書く (`check` 等で設定を書き出す経路を崩さないため)
- **鍵は設定の外、ns ごとの鍵束ファイル** (DR-0030 §5)。鍵の追加・失効は設定と寿命が違う (ローテで 180 日ごとに増減する) ので設定に埋めない。鍵束は `jwt` の検証と `issued` の署名で共用する
- **鍵束の形**: 1 行 1 JWK の jsonl。各行は `{"kty":"OKP","crv":"Ed25519","kid":…,"d":…,"x":…}` で、秘密鍵 (`d`) を含む。検証の方式は行の `kty` / `crv` から決め、Ed25519 以外の行は読み込みエラー。ファイルが正本で、追加は行の追記、失効は行の削除
- **反映**: gateway は起動時に読んでメモリに持ち、検証のたびに mtime を見て、変わっていれば読み直す。restart は要らない

### `extends` (DR-0013) との相性

`keys_file` はパス 1 つの欄なので、派生側で書けばファイルごと差し替わる (鍵単位のマージはしない。鍵束の中身は設定でなくファイルの責務)。どの鍵が有効かは「その ns の `keys_file` が指す 1 ファイルの中身」だけで決まり、土台と派生のどちらで定義したかを追う必要がない。

## 2. 検証の実装

### crate

| 候補 | ヘッダの alg を信用しない | 依存 | 評価 |
|---|---|---|---|
| `jsonwebtoken` | `Validation::new(Algorithm::EdDSA)` で alg を固定でき、ヘッダの alg が違えば弾く。ただし「kid → 鍵」の引き当てとヘッダ解析の順序は呼び出し側で書く | `ring` (または版により `aws-lc-rs`)、`simple_asn1`、`pem` 等。既存 workspace に `ring` は無い | 動くが重い。Ed25519 だけのために汎用 JOSE を入れる |
| `ed25519-dalek` + 自前の JWS compact 解析 | 自前なのでヘッダの `alg` は「kid ごとの設定値と一致するか」の照合にしか使わない形を構造で保証できる | `ed25519-dalek` (+ `curve25519-dalek`、`sha2`)。`base64` / `serde_json` は既存 | **採用**。JWS compact は `b64(header).b64(payload).b64(sig)` の分割と、署名入力 = 先頭 2 段のバイト列だけで、書く量は 100 行程度 |
| `jwt-simple` | 鍵型が alg を決めるので安全側 | `ed25519-compact` 等、中程度 | 候補にはなるが API が大きく、`issued` で発行側も書く時に自前の方が揃う |

`ed25519-dalek` + 自前を採る。理由: DR-0030 §6 の「検証アルゴリズムは鍵の `kty` / `crv` から決め、ヘッダの `alg` を信用しない」は、ライブラリの `Validation` に任せるより **検証関数の引数に alg を持たせず、kid から引いた鍵の型 (`VerifyingKey`) が alg そのもの**という形の方が崩れない。`issued` (手順 5) で署名側を書く時も同じ crate で済む。`verify_strict` を使い、小位数点・非正準な署名を弾く。

置き場: 検証ロジックは `gateway-core::ns` (または `gateway-core::ns::jwt`)。LLM の語彙を含まないので汎用層に置ける。`ed25519-dalek` は `gateway-core` の依存になる。

### 検証の順序

1. `Bearer ` を剥がす (現行どおり `Bearer` 無しも受けるかは下記)。`.` で 3 段に割れなければ拒否
2. ヘッダを base64url 復号 → JSON。`kid` が無い / 鍵束に無い → 拒否。`alg` が kid の鍵種から決まる値 (Ed25519 なら `EdDSA`) と一致しない → 拒否 (`none` はここで落ちる。ヘッダの `alg` は照合にだけ使い、検証の方式の選択には使わない)。`crit` があれば拒否 (理解できない拡張を黙って無視しない)。`typ` は見ない (付けない実装が多い)
3. 署名を検証 (署名入力は受け取った先頭 2 段の生バイト列。再エンコードしない)
4. 署名が通ってから payload を JSON として読み、claim を検査

### claim

| claim | 扱い |
|---|---|
| `exp` | 必須。`now > exp + skew` で拒否 |
| `iat` | 任意。あれば `iat <= now + skew` であること (未来発行を拒否) |
| `nbf` | あれば `now + skew < nbf` で拒否 |
| `sub` | 必須。空文字列は拒否。`Principal.subject` に入れる |
| `iss` / `aud` | ns 設定に書いた時だけ照合。書いていなければ値があっても見ない |
| `jti` | 見ない (`issued` の refresh 再利用検知で使う時に足す) |
| その他 | 無視 |

- **寿命上限**: `exp - now <= max_ttl` で測る (`iat` は任意なので頼らない)。`iat` があれば `exp - iat <= max_ttl` も見る。署名者 = 鍵保持者なので、上限は「鍵保持者の誤りを弾く」歯止めであって攻撃対策ではない
- **時計の揺れ**: skew は固定 60 秒 (設定欄にしない。ns ごとに変えたい場面が見えない)
- **`sub` の文字種**: events / stats のキーになるので、長さ上限 (例 128) と制御文字の拒否を入れる

### 失敗時の応答

- 状態は 401。本文と `WWW-Authenticate: Bearer error="invalid_token"` だけで、**どの検査で落ちたかは応答に出さない** (kid の存在・期限切れ・署名不一致を外から区別させない)
- 理由は gateway 内の観測に残す。`Authorization` に `Rejected(Reason)` を足し (`WrongToken` は `Rejected(Reason::Mismatch)` 相当に畳むか並べるかは実装時に決める)、理由は `unknown_kid` / `bad_signature` / `expired` / `ttl_exceeded` / `claims` / `malformed` 程度の粗さ。これは tracing のログにだけ出す。期限切れも応答では区別しない (利用者の導線は runbook で補う)
- 現行の `rejection(ns, ns_name, headers)` (名乗り方の違いを案内する経路) は token 方式のもの。jwt 方式でも同じ案内を出すかは実装時に読む

### 名乗り方

Claude Code は `ANTHROPIC_AUTH_TOKEN` を `Authorization: Bearer` で送る。LLM 経路で `x-api-key` を受けている箇所があるなら、jwt 方式でもそこから取るかを揃える (現行の token 方式の受け口と同じ集合にする)。

## 3. helper CLI

### サブコマンド

DR-0028 の体系 (`daemon` / `service` / `upstream` は子を持つレベル) に合わせ、子を持つ `auth` を足す:

```
llm-gateway auth                     # help を出す
llm-gateway auth keygen [--kid <kid>]
llm-gateway auth jwks [--key <jwks.jsonl|->] [--kid <kid>]
llm-gateway auth sign [--key <jwks.jsonl|->] [--kid <kid>] --sub <subject> --ttl <dur> [--iss <iss>] [--aud <aud>]...
```

- `keygen`: Ed25519 鍵ペアを作り、**JWK 1 行を標準出力**に出す (`{"kty":"OKP","crv":"Ed25519","kid":"...","d":"...","x":"..."}`)。鍵束への追記は利用者が `>>` で行う。`--kid` 省略時は日付 + 乱数 (例 `2026-09-24-3f9a`)。ファイルへの直接書き出し (`--out`) は持たない — 持たせると「CLI が秘密をどこかに置いた」になり、鍵束の窓口がファイルと CLI の 2 つに割れる。新規の鍵束は umask 次第で 644 で作られるので、runbook に `chmod 600` を書く
- `jwks`: 鍵束から **公開鍵だけ**を抜き出し、JWKS JSON (`{"keys":[…]}`、`d` を含まない) を 1 行で出す。`--kid` で 1 本に絞れる (無い kid はエラー)。配る形へのラップはこの出力を使う側で行う
- `sign`: 鍵束から `--kid` の行の秘密鍵で JWT を鋳造して標準出力に 1 行。`--kid` を省いた時、鍵束が 1 行ならそれを使い、複数行ならエラー (どの kid で署名したかを利用者が意識しないまま配る事故を避ける)。`iat = now`、`exp = now + ttl`。`--ttl` は `max_ttl` と同じ書式 (`90d` 等)。設定ファイルは読まない (`max_ttl` との照合は gateway 側の責務)。`--key -` (既定) で標準入力から読めるので、鍵束を 1Password から流す運用 (`op read ... | llm-gateway auth sign ...`) も取れる
- `issued` の bootstrap 発行 (DR-0030 §6) は手順 5 で `auth issue` 等として足す。refresh token を出す点で `sign` とは責務が違うので、`sign` に混ぜない
- help はテキスト (DR-0028 §8)、文言は英語 (DR-0008)。引数なしで help、ロングオプションのみ、`--` セパレータ対応 (cli-design-preferences)。この CLI に completion の定義は無い

### Claude Code 向けの運用 (runbook の骨子)

鋳造:

1. `llm-gateway auth keygen --kid claude-mbp-2026-09 >> ~/.config/llm-gateway/keys/claude.jwks.jsonl` → `chmod 600` (新規作成時)。設定の `[ns.claude]` に `keys_file` を書く (設定を初めて変える時だけ daemon の rolling restart)
2. `llm-gateway auth sign --key ~/.config/llm-gateway/keys/claude.jwks.jsonl --kid claude-mbp-2026-09 --sub kawaz-mbp --ttl 180d` の出力を、各機械の Claude Code `settings.json` の `env.ANTHROPIC_AUTH_TOKEN` に貼る。`sub` は機械 (または用途) ごとに分けると events で見分けられる

kid は機械ごとに 1 本。ローテ (既定は鍵 180 日、token の ttl は鍵周期 + 猶予):

1. 新しい鍵を keygen して鍵束に**追記** (旧 kid の行は残す)。restart は要らない (次のリクエストの検証で mtime の変化を見て読み直す)
2. 新 kid で各機械の token を鋳造し直して貼り替え。走行中の Claude セッションは起動時の env を持ち続けるので、全セッションの再起動を待つ
3. events で旧 kid の利用が 0 になったのを確かめて旧 kid の行を鍵束から削除。これが失効

漏洩時: 該当 kid の行を即削除 (次のリクエストからその kid の token は全て失効)。別 kid で鋳造した他の機械は影響を受けない (kid を機械ごとに分ける理由)。鍵束ファイル自体が漏れた疑いがある時は全行が漏洩扱い。

## 4. Principal の下流

- **events**: `request` (DR-0012) と `passthrough` に `subject` と `kid` を足す。どちらも `skip_serializing_if = "Option::is_none"` で、token 方式・無検査の ns では欄自体が出ない (既存の読み手を壊さない。DR-0012 の「欄自体は必ず出す」は `origin` の規定で、新欄を常時出す義務ではない)。`request` と対になる完了の知らせ (DR-0012 の 2 通目) にも同じ値を載せる (DR-0012 の「見る側に 2 通の突き合わせを強いない」)
- **認証失敗**: events に出さない。拒否理由はログだけ (未認証の相手の入力で events の流量が増える口を作らない)
- **stats**: subject 軸は足さない。軸を増やすと保存形式と CLI の `--by` が増える。events に subject が載るので外部集計はできる
- **レート制限 / allowlist**: 今は ns 単位。subject 単位の枠は本手順の範囲外 (Principal に揃えたので後から足せる、が境界)

## 5. 段分け

Phase 0 の 3 行 (完了条件):

- 目的: ns の `auth` に `jwt` 方式を足し、helper CLI で鋳造した Ed25519 JWT で Claude Code が ns を通れるようにする
- 完了の判定: `auth = "jwt"` の ns に対し、`keys_file` の鍵束から `auth sign` で作った token が通り、期限切れ・未知 kid・alg 違い・署名改竄・寿命超過が 401 になり、鍵束から行を消すと restart 無しで 401 になる e2e テストが `just ci` で緑。既存の `auth_token` 設定は無変更で通る
- 範囲外: `issued` (発行の HTTP endpoint、bootstrap、kid ごとの最終発行時刻)、鍵束の backend 差し替え (Store 層 / cache-warden)、設定ファイル自体の稼働中再読込、subject 単位の stats / 枠

段 (各段 `just ci` 緑で閉じる):

1. **`NsAuth` を enum 化** (振る舞い不変)。`Open` / `Token` の 2 枝、生の欄からの畳み込み、方式と欄の食い違いの設定エラー。既存テスト全部そのまま緑 + 食い違いのテスト
2. **jwt 検証** (`gateway-core`)。`ed25519-dalek` 導入、JWS 解析、claim 検査、`Rejected(Reason)`。単体テストで上の拒否 5 種 + `none` + `crit` + 未来 `iat` + skew 境界。テスト用の鍵はテスト内で生成
3. **設定と経路の接続**。`auth = "jwt"` / `keys` / `max_ttl` / `iss` / `aud` の読み込み、LLM 経路と passthrough の両方で jwt を通す、401 + `WWW-Authenticate`、`extends` で `keys` が kid ごとにマージされるテスト。稼働中の設定再読込の有無を確認し、無ければ「失効は restart で反映」を MANUAL に明記する。e2e で Claude 形式のリクエストが通る
   - 段 1〜3 実施済み: `NsAuth` は `Open` / `Token` / `Jwt` の enum、検証は `gateway_core::ns::jwt::JwtAuth`、失敗は `Authorization::Rejected(Reason)` でログだけに理由を出す。設定は起動時にしか読まないので、失効は restart で反映 (MANUAL に明記)
4. **Principal を events へ**。`request` / 完了 / `passthrough` に `subject` / `kid`。無い時は欄が出ないことをテストで縛る
5. **helper CLI** `auth keygen` / `jwks` / `sign`、help、completion。`sign` の出力を段 2 の検証に通す往復テスト (CLI と検証器が同じ規約で合っていることの証明)
   - 段 4 / 5 実施済み: 知らせの `request` / `response` / `passthrough` に `subject` / `kid` (jwt の ns だけ)。CLI `auth keygen` / `jwks` / `sign` (鍵の生成・JWK・署名は `gateway_core::ns::jwt` に置き、CLI は呼ぶだけ)。この CLI には completion の定義が無いので、足すものは無い
6. **docs**: README / 設定例、runbook (`docs/runbook/` 等、既存の置き場に合わせる)、DR-0030 の Status に手順 4 実装済みを記す
   - 段 6 実施済み: runbook `docs/runbooks/ns-auth-jwt-rotation.md` (初回の鋳造 / ローテ / 漏洩時 / 切り分け)、DR-0030 §6 から参照

7. **鍵束を `keys_file` の jsonl に移す**。段 1〜6 の現在の実装 (鍵は設定の `[ns.<ns>.keys.<kid>]` 表、`alg` / `public`、起動時にだけ読む) を DR-0030 §5 / §6 の形に置き換える。各項の後ろの括弧が現在のコードの位置

   1. **設定欄** (`crates/llm-gateway/src/config.rs`)
      - 生の欄の `keys: BTreeMap<String, KeySpec>` と `KeySpec` (`[ns.<name>.keys.<kid>]` の alg / public) を撤去し、`keys_file: Option<PathBuf>` を足す。他のパス欄と同じく `#[serde(default, with = "path_expand::serde_opt_path")]` で `~` と環境変数を開く
      - `jwt_auth(keys, max_ttl, iss, aud)` は `keys_file` を受け、鍵束を読んで `JwtAuth` を作る。食い違いの文言を更新する: `auth = "jwt"` で `keys_file` が無ければ ``auth = "jwt" needs `keys_file` (one JWK per line, as `llm-gateway auth keygen` writes)``、`jwt` 以外の方式で `keys_file` / `max_ttl` / `iss` / `aud` があれば ``"`keys_file` / `max_ttl` / `iss` / `aud` belong to auth = \"jwt\"; add it or remove them"``
      - 起動時に鍵束が読めない (無い・権限・不正行) のは設定エラーで起動を止める (無検査で上がる / 全拒否で上がる、のどちらも黙って起きると気づけない)
      - `Serialize` (生の欄へ戻す経路、今は kid ごとの表を書き戻している) は `keys_file` のパスを書き戻す形にする。鍵束の中身は書き出さない
      - 旧形式の `[ns.<ns>.keys.<kid>]` が残った設定は、未知の欄として読み込みエラーにする (serde が `deny_unknown_fields` でない場合は明示的に検出し、``"`keys` is replaced by `keys_file`; move the keys into a jsonl file (see docs/runbooks/ns-auth-jwt-rotation.md)"`` のように移行先を示す。これは error message に限った移行誘導)
   2. **鍵束の読み込み** (`crates/gateway-core/src/ns/jwt.rs`)
      - `pub fn read_key_ring(text: &str) -> Result<BTreeMap<String, ed25519_dalek::SigningKey>, String>` 相当を足す。空行は読み飛ばし、各行を既存の `read_private_jwk` で読む (`kty` / `crv` が Ed25519 でない・`d` が無い・`x` が `d` と合わない行はエラー)。行に `kid` が無ければエラー (鍵束では kid が引き当ての鍵)。**kid の重複はエラー** (後勝ちにすると、追記したつもりの新鍵が古い行を黙って覆う)。エラー文には行番号を入れ、行の中身 (秘密鍵) は出さない
      - 検証側は秘密鍵を持つ必要が無いので、`JwtAuth` には今どおり `VerifyingKey` の表を渡す (読み込んだ直後に `verifying_key()` へ落とす)。署名側 (CLI の `sign`、後の `issued`) だけが `SigningKey` を使う
      - `parse_public_key` (base64url 生 32 byte の読み取り) と `public_key_text` は設定から公開鍵を読む経路が無くなるので、残る呼び出し元 (`public_jwk` の `x` 等) を grep で確かめて不要なら消す
   3. **稼働中の再読込**
      - **検証の入口で読み直す** (per-request check。監視用の task は立てない)。鍵束は `gateway_core::ns::jwt::KeyRing` (`keys_file` のパス、最後に読んだ時の mtime、kid → 公開鍵の表) が持ち、`JwtAuth` は `ring: KeyRing` と `max_ttl` / `iss` / `aud` を持つ。`JwtAuth::verify` (`NsAuth::verify` の `Jwt` 枝から呼ばれる) の入口で `KeyRing::current()` が `keys_file` を stat し、記録した mtime と違えば鍵束を読み直して表と mtime を差し替え、その時点の表を返す。署名・claim の検査はその表に対して行う。mtime の取り方は `FileStore::version` (`crates/gateway-core/src/credential/file.rs`、`modified()` の UNIX epoch からの ns) と揃える。stat は 1 リクエスト 1 回で、中身は mtime が変わった時だけ読む
      - `JwtAuth` は今 `Config` の中に不変で居て、`Gateway::namespace()` が `&Namespace` を返し `NsAuth::verify(&self, …)` が読むだけ。`&self` のまま差し替えられるよう、`KeyRing` の中で鍵の表と mtime を `std::sync::RwLock` で内部可変にする (`RwLock<(Option<u64>, Arc<BTreeMap<String, VerifyingKey>>)>` 等。読み手はロックを短く取って `Arc` を clone し、検証はロックの外で行う。`arc-swap` は足さない)。同時に複数のリクエストが変化を見た時は、書きロックを取った後で stat し直し、記録する mtime と読む中身を同じ stat に揃えて、読み直しを 1 回に抑える。`KeyRing` の `Clone` / `PartialEq` は手書き (パスと、その時点の表で比べる) で、`JwtAuth` はそれを使って derive する
      - 読み直しで失敗した (不正行・kid 重複・一時的に読めない) 時は **前の鍵束を持ち続け**、ログに警告を出す。書きかけのファイルを拾って全鍵を失う事故を避ける。mtime の粒度で同時刻に 2 度書くと取りこぼしうる点は DR-0030 Consequences の静的 secret と同じ制約として受ける
      - 利用者が `>>` や手編集で書くので、書き込みは rename 経由とは限らない。追記の途中を読むと最終行が欠けて不正行になるが、上の「失敗したら前の束を保つ」で次のリクエストで拾い直せる
   4. **CLI** (`crates/llm-gateway-cli/src/auth.rs`、help は `crates/llm-gateway-cli/src/help.rs`)
      - `keygen`: 出力は今と同じ JWK 1 行 (変更なし)。help に「`>>` で鍵束に追記する」例を足す
      - `jwks`: 引数を `--key` / `--kid` だけにする (`--ns` と `--format` を撤去、TOML 断片の `toml_key` も撤去)。入力を鍵束として読み、全行 (`--kid` があればその 1 行、無ければエラー) の公開鍵を `{"keys":[…]}` で出す
      - `sign`: 入力を鍵束として読み、`--kid` の行で署名する。今の `read_key` は「`--kid` が JWK の kid より優先する」(kid の上書き) なので、意味を「鍵束から選ぶ」に変える。`--kid` 省略時は 1 行ならそれ、複数行なら ``"the key ring has N keys; give --kid (one of: a, b)"`` のようなエラー。無い kid もエラー
      - `--key -` (既定、標準入力) は維持
   5. **テスト**
      - `gateway-core`: jsonl の読込 (複数行・空行)、不正行 (JSON でない / Ed25519 でない / `d` 無し / `x` 不一致 / `kid` 無し) がエラーで、文に行番号があり秘密鍵が出ない、kid 重複がエラー
      - `llm-gateway` の設定: `keys_file` の読み込みと `~` 展開、`auth = "jwt"` で `keys_file` 無しがエラー、`jwt` 以外で `keys_file` がエラー、旧 `[ns.<ns>.keys.<kid>]` が移行先を示すエラー、`extends` の派生で `keys_file` を書けばファイルごと差し替わる (今の `keys_from_a_base_and_a_derived_file_are_merged` を置き換える)
      - 再読込: 鍵束から行を消すとその kid の token が restart 無しで 401 になる、行を足すと通る、不正な書き換えでは前の鍵束が残る。いずれも鍵束を書き換えて mtime を進めた (試験では `File::set_modified` で明示的に進め、時刻の粒度に頼らない) 次の `verify` で反映されることを確かめる。mtime が変わらなければ読み直さない (中身を書き換えても mtime を戻せば前の表で検証される) ことも縛る
      - CLI (`crates/llm-gateway-cli/tests/auth.rs`): `keygen >> ring` を 2 回 → `sign --kid` の出力が gateway 側の検証 (`JwtAuth::verify`) を通る往復、`--kid` 省略で 1 行なら通り複数行ならエラー、`jwks` の出力に `d` が無い
   6. **docs**
      - `docs/MANUAL-ja.md` / `docs/MANUAL.md` の「`jwt` の namespace」(``### `jwt` の namespace`` / ``### `jwt` namespaces``) を `keys_file` と鍵束の形 (1 行 1 JWK、600、mtime で読み直す) に書き換え、「失効は restart で反映」の記述を「鍵束の行を消すと次のリクエストから反映」に改める。「`auth` — `jwt` の namespace の鍵と token」(``### `auth` — keys and tokens for `jwt` namespaces``) の例と引数一覧を §3 の形にする
      - runbook `docs/runbooks/ns-auth-jwt-rotation.md` の各手順を §3「Claude Code 向けの運用」の形に: 「Open の ns を無停止で `jwt` に移す」は鍵束を先に作って token を配ってから `auth = "jwt"` と `keys_file` を書いて rolling restart、「初回の鋳造」は `keygen >>` + `chmod 600` + `sign --kid`、「ローテ」と「漏洩時」は行の追記・削除で restart 無し (反映は次のリクエストから)、「失敗時の切り分け」に鍵束の読み込みエラー (起動時は起動失敗、稼働中は警告ログで前の束を保持) を足す
   7. **`check` の有効 kid 一覧**: `crates/llm-gateway-cli/src/check.rs` は今 ns ごとに routing / aliases / cache の数を出している。`jwt` の ns について `keys_file` のパスと有効な kid の一覧を同じ並びに足す (鍵束の読み込みエラーもここで出る)。extends のどのファイルが定義元かを追う必要は無くなったが、「どの鍵束を指し、中に何があるか」を 1 コマンドで確かめる口として価値がある。秘密鍵は出さない

`issued` との境界: 検証器 (`JwtAuth`) の検査は「kid → 公開鍵」の表を受けるだけにし、表の出どころ (ファイルか、与えた表か) は `KeyRing` が持つ。鍵束は `jwt` と `issued` で共用し、手順 5 は同じ鍵束の秘密鍵で署名する側 (kid ごとの最終発行時刻、token endpoint、bootstrap) を足す。

## 6. リスク・未確認

- **長寿命 JWT の漏洩窓**: 失効手段は鍵束からの kid の行の削除だけなので、漏れた token は行を消すまで ns の全権限で通る。token 単位の失効 (jti の拒否リスト) は持たない。緩和は kid の粒度を細かくする (機械ごと) ことと、ns の allowlist / 枠。漏れた token は `settings.json` (平文) から漏れるのが典型で、これは現行の固定 token と同じ危険度。jwt で良くなるのは「どの機械の token か (sub / kid) が events で分かる」「1 台分だけ失効できる」点
- **鍵束の秘密鍵が平文ファイルに居る**: 鍵束は検証と署名の共用で秘密鍵を含む (600)。漏れればその ns の token を偽造でき、allowlist も ns の区分も迂回される (DR-0030 Consequences)。cache-warden 稼働までは平文の期間で、runbook の漏洩時手順 (全行の入れ替え) がその歯止め
- **同じ mtime での書き換え**: 版を mtime だけで比べるので、時刻の粒度の内で 2 度書くと 2 度目を取りこぼしうる (DR-0030 Consequences の静的 secret と同じ制約)。行を消したのに通る時は、鍵束を `touch` し直せば次のリクエストで読み直す
- **書きかけの鍵束**: `>>` の追記は rename 経由でないので、読み手が途中を見うる。1 行でも不正なら束全体を不採用にし、0 行 (空・空行だけ) の束もエラーにするので、追記途中の欠けた行や truncate 直後を拾っても前の束を保つ。先頭の完結した行だけが書かれた途中 (手編集の上書き) は形式上見分けられないので、行の削除は一時ファイルに書いて `mv` で置き換える (rename で原子的に) 運用とし、runbook と MANUAL に書く
- **検証経路の同期 I/O**: 検証経路の stat / read は同期 I/O。stat は µs 級で、read は mtime が変わった時だけなので spawn_blocking に出さない。読み直しに失敗した mtime も記録し、同じ mtime の間は再試行も警告もしない (失敗が続く間、リクエストごとに read と warn を重ねない)
- **時計**: gateway 機の時計が大きくずれると全 token が一斉に失効 / 通過する。skew 60 秒を超えるずれは NTP 前提で扱わない
- **`ed25519-dalek` の版**: 2 系の `verify_strict` を前提にしている。workspace の他依存 (`sha2` 等) との版衝突は未確認
- **Claude Code の挙動**: `ANTHROPIC_AUTH_TOKEN` に 300 byte 程度の JWT を入れて問題が無いかは未確認 (長さ制限は無いと見ているが、実機で 1 回確かめる)。`ANTHROPIC_AUTH_TOKEN` があるとサブスクとしての振る舞いをやめる件 (config.rs のコメント) は token 方式と同じで、jwt 方式で変わらない

## 裁定済み (統括 2026-09-24)

(2 / 3 / 6 / 10 は 2026-09-29 の kawaz 裁定で改まった。下の「裁定済み (kawaz 2026-09-29)」を参照)

1. 設定は平置き: `auth = "jwt"`、ns 直下に `keys` / `max_ttl` / `iss` / `aud`。`auth_token` だけの既存設定は無変更で token 方式
2. `keys` は kid をキーにした表 `[ns.<name>.keys.<kid>] alg = "EdDSA", public = "<base64url>"` (`[secrets.<id>]` / `[upstreams.<name>]` と同じ流儀。extends のマージが鍵ごとに効く。失効は定義しているファイルから消す)
3. 公開鍵は base64url (32 バイト) だけ。JWK は CLI の `auth jwks` の出力形式
4. `exp` と `sub` は必須、`iat` は任意 (あれば now + skew 以下)。skew は 60 秒固定
5. 401 は理由を区別しない (ログに残す)
6. CLI は `auth keygen` / `auth jwks` / `auth sign`。秘密鍵は標準出力のみ (`--out` 無し)、`sign --key -` で標準入力
7. events: `request` / `passthrough` に `subject` / `kid` を Option の欄で足す (無ければ欄ごと出さない)。認証失敗は events に出さない
8. `ed25519-dalek` + 自前 JWS 解析
9. kid は機械ごと、鍵ローテは 180 日を runbook の既定
10. 稼働中の設定再読込の有無は段 3 で実装者が確認し、無ければ「失効は restart で反映」を MANUAL に明記
11. stats に subject 軸は足さない

## 裁定済み (kawaz 2026-09-29、DR-0030 §5 / §6)

1. 鍵は設定の `[ns.<ns>.keys.<kid>]` 表でなく、`[ns.<ns>] keys_file = "<path>"` (必須、`~` / 環境変数を開くパス欄) が指す ns ごとの鍵束ファイル。`extends` の派生で上書きすればファイルごと差し替わり、鍵単位のマージはしない
2. 鍵束は `<ns>.jwks.jsonl`、1 行 1 JWK (`auth keygen` の出力そのもの、秘密鍵込み、600)。ファイルが正本で、追加は追記・失効は行の削除。将来 daemon 経由の窓口を足しても正本はファイル (Store 層の 1 品目)
3. gateway は起動時に読んでメモリに持ち、検証のたびに `keys_file` の mtime を見て、変わっていれば読み直す (per-request check、監視用の task は立てない)。読み直しに失敗したら前の束を保って警告する。restart は要らない
4. 検証の方式は鍵の `kty` / `crv` から決め、ヘッダの `alg` は信用しない。Ed25519 以外の行は読み込みエラー
5. `jwt` の検証と `issued` の署名で同じ鍵束を共用する
6. CLI: `auth keygen` は JWK 1 行を標準出力 (追記は利用者が `>>`)。`auth jwks --key <jwks.jsonl> [--kid <kid>]` は公開鍵だけの JWKS JSON (TOML 断片は出さない)。`auth sign --key <jwks.jsonl> --kid <kid> --sub … --ttl …` で秘密鍵を選び、`--kid` 省略時は 1 行ならそれ・複数ならエラー。`--key -` の標準入力は維持
