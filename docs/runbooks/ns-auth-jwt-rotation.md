# Runbook: ns 認証 `jwt` の鍵の鋳造・ローテ・失効

- Last Updated: 2026-09-29

## 適用ケース

- Claude Code などのクライアントに、`auth = "jwt"` の namespace を使わせ始める時 (初回の鋳造)
- 鍵の周期 (既定 180 日) が来た時 (ローテ)
- 秘密鍵か token が漏れた、または漏れた疑いがある時 (失効)

## 前提

- `llm-gateway` の CLI が手元にあること (`auth keygen` / `auth jwks` / `auth sign`)
- 設定ファイルを編集でき、`llm-gateway daemon restart` を打てること
- kid は **機械 (または用途) ごとに 1 本**。1 台分だけを失効できるようにするため
- **設定は起動時にしか読まない**。鍵の追加・削除は restart で反映される。rolling restart の間は、まだ再起動していない unit で古い鍵が通る
- `keys` は kid ごとの表なので、`extends` した派生のファイルに足した鍵は土台に加わる。**失効は、その kid を定義しているファイルから消す** (派生側からは土台の kid を消せない)

## 手順

### `auth` 未設定 (Open) の ns を無停止で `jwt` に移す

認証方式は ns 単位で 1 つなので、`auth = "jwt"` を書いた瞬間、その ns を使う走行中セッションは token を持っていなければ 401 になる (手前の Caddy は 401 で fail over しない)。ns が Open (`auth` 未設定) のうちは gateway が `Authorization` を検査しないので、**先にクライアント全部へ JWT を配り、走行中セッションが入れ替わるのを待ってから config を切り替える**。この「配布済みだが未検査」の期間が移行期間で、token と jwt を同時に受ける方式を gateway に足す必要はない。

1. 鍵を作り、公開鍵の TOML 断片を作っておく (初回の手順 1〜2 の前半。**config には貼らない**。`keys` は `auth = "jwt"` と同時にしか書けず、片方だけだと `check` が落ちる)
2. JWT を鋳造し、クライアントに貼る (初回の手順 4)。Claude Code は `settings.json` の `env.ANTHROPIC_AUTH_TOKEN`、codex は `[model_providers.<id>]` の `http_headers = { Authorization = "Bearer <JWT>" }`。この時点で新旧どちらの値でも通る
3. その ns を使う走行中セッションが自然に再起動され切るのを待つ (急がない)。残っているものは `ccmsg peers` 等で数える
4. ns ごとに `auth = "jwt"` / `max_ttl` / `keys` を config に足し、`check` → unstable から restart (初回の手順 3)。**貼った JWT が正しいかはここで初めて検査される**ので、unstable で `claude -p --model claude-haiku-4-5-20251001 'Reply with the single word: ok' < /dev/null` の疎通を見てから stable に進む
5. 初回の手順 5 で `subject` / `kid` の記録を確かめる

秘密鍵の置き場: `~/.config/llm-gateway/private/<kid>.jwk` (dotfiles では `/config/llm-gateway/private` を gitignore 済み、ディレクトリは 700)。公開鍵の断片は同じ場所の `<ns>.keys.toml`。パスワードマネージャへ移すなら移した後にファイルを消す。

### 初回の鋳造

1. **鍵ペアを作る。** 秘密鍵は標準出力にだけ出る
   ```bash
   llm-gateway auth keygen --kid claude-mbp-2026-09 > claude-mbp-2026-09.jwk
   chmod 600 claude-mbp-2026-09.jwk
   ```
   期待結果: 1 行の JWK (`{"kty":"OKP","crv":"Ed25519","kid":"claude-mbp-2026-09","d":…,"x":…}`)。保管はパスワードマネージャに移し、ファイルは最後に消す

2. **公開鍵を設定に足す。**
   ```bash
   llm-gateway auth jwks --ns claude < claude-mbp-2026-09.jwk
   ```
   期待結果: `[ns.claude.keys.claude-mbp-2026-09]` の `alg` / `public`。これを設定の `[ns.claude]` (`auth = "jwt"`、`max_ttl = "400d"` 等) の下に貼る

3. **読み込ませる。**
   ```bash
   llm-gateway check --config <設定ファイル>
   llm-gateway daemon restart --all
   ```
   期待結果: `check` がエラーを出さず、restart が全 unit で healthz まで戻る

4. **token を鋳造して、クライアントに持たせる。**
   ```bash
   llm-gateway auth sign --sub kawaz-mbp --ttl 180d < claude-mbp-2026-09.jwk
   ```
   期待結果: 1 行の JWT。Claude Code なら `settings.json` の `env.ANTHROPIC_AUTH_TOKEN` に入れ、`ANTHROPIC_BASE_URL` を `…/ns-claude` にする。`--sub` は機械ごとに分けると、知らせ (`/llm-gateway/events` の `subject` / `kid`) で見分けられる。`--ttl` は `max_ttl` 以下

5. **確かめる。** 新しい Claude Code のセッションで 1 往復し、`/llm-gateway/events` の `request` に `subject` / `kid` が載ることを見る

### ローテ (既定 180 日)

1. **新しい kid の鍵を作り、`keys` に足す** (旧 kid は残す)。初回の手順 1〜3 と同じ
2. **新しい kid で各クライアントの token を鋳造し直して貼り替える** (初回の手順 4)。走行中の Claude Code のセッションは起動時の env を持ち続けるので、全セッションの再起動を待つ
3. **旧 kid の利用が 0 になったのを確かめる。** `/llm-gateway/events` で旧 kid の `kid` が来なくなったこと
4. **旧 kid を定義しているファイルから消し、restart する。** これが旧 kid の失効

### 漏洩時

1. **漏れた kid を、定義しているファイルから即座に消し、restart する。** その kid で鋳造した token は全て通らなくなる。別の kid で鋳造した機械は影響を受けない
2. 必要なら、その機械のために新しい kid で初回の手順 1〜4 をやり直す
3. 秘密鍵が漏れた場合は、その鍵で鋳造した token が他にもあれば同じく全て失効している (kid 単位の失効)

## 失敗時の切り分け

| 症状 | 原因 | 対処 |
|---|---|---|
| クライアントが 401 (`WWW-Authenticate: Bearer error="invalid_token"`) | kid が設定に無い / 期限切れ / 寿命が `max_ttl` 超え / 署名が合わない / `iss` / `aud` 不一致 | 応答では区別しない。gateway のログの `refused a namespace token` の `reason` を見る (`unknown_kid` / `expired` / `ttl_exceeded` / `bad_signature` / `claims` / `alg_mismatch` / `malformed` 等) |
| 鍵を消したのに通る | restart していない、または別のファイルがその kid を定義している (extends の土台) | `daemon restart --all`。`extends` の土台と派生の両方で kid を探す |
| 足した鍵で通らない | restart していない、`public` の貼り間違い | `auth jwks` の出力と設定を突き合わせ、restart する |

## 関連

- DR-0030 §6 (ns 認証の方式と `jwt` の規定)
- `docs/design/ns-auth-jwt.md` (実装計画)
- MANUAL の「`jwt` namespaces」「`auth`」の節
