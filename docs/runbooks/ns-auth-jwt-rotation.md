# Runbook: ns 認証 `jwt` の鍵の鋳造・ローテ・失効

- Last Updated: 2026-09-29

## 適用ケース

- Claude Code などのクライアントに、`auth = "jwt"` の namespace を使わせ始める時 (初回の鋳造)
- 鍵の周期 (既定 180 日) が来た時 (ローテ)
- 秘密鍵か token が漏れた、または漏れた疑いがある時 (失効)

## 前提

- `llm-gateway` の CLI が手元にあること (`auth keygen` / `auth jwks` / `auth sign`)
- 鍵束は ns ごとに 1 ファイル、1 行 1 JWK の jsonl (秘密鍵込み、600)。置き場の例は `~/.config/llm-gateway/keys/<ns>.jwks.jsonl`。ファイルが正本で、鍵の追加は行の追記、失効は行の削除。追加は `>>` の追記、削除は一時ファイルに書いて `mv` で置き換える (rename で原子的に)
- **鍵束の変更に restart は要らない**。gateway は検証のたびに鍵束の mtime を見て、変わっていれば読み直す (反映は次のリクエストから)。restart が要るのは設定ファイル (`auth = "jwt"` / `keys_file` 等) を変えた時だけ
- kid は **機械 (または用途) ごとに 1 本**。1 台分だけを失効できるようにするため
- 以下の例は ns が `claude`、鍵束が `~/.config/llm-gateway/keys/claude.jwks.jsonl`

## 手順

### `auth` 未設定 (Open) の ns を無停止で `jwt` に移す

認証方式は ns 単位で 1 つなので、`auth = "jwt"` を書いた瞬間、その ns を使う走行中セッションは token を持っていなければ 401 になる (手前の Caddy は 401 で fail over しない)。ns が Open (`auth` 未設定) のうちは gateway が `Authorization` を検査しないので、**先に鍵束を作ってクライアント全部へ JWT を配り、走行中セッションが入れ替わるのを待ってから設定を切り替える**。この「配布済みだが未検査」の期間が移行期間で、token と jwt を同時に受ける方式を gateway に足す必要はない。

1. 鍵束を作る (初回の手順 1)。**設定にはまだ書かない**。`keys_file` は `auth = "jwt"` と同時にしか書けず、片方だけだと `check` が落ちる
2. JWT を鋳造し、クライアントに貼る (初回の手順 2〜3)。Claude Code は `settings.json` の `env.ANTHROPIC_AUTH_TOKEN`、codex は `[model_providers.<id>]` の `http_headers = { Authorization = "Bearer <JWT>" }`。この時点で新旧どちらの値でも通る
3. その ns を使う走行中セッションが自然に再起動され切るのを待つ (急がない)。残っているものは `ccmsg peers` 等で数える
4. ns に `auth = "jwt"` / `keys_file` / `max_ttl` を書き、`check` → unstable から rolling restart (初回の手順 4)。**貼った JWT が正しいかはここで初めて検査される**ので、unstable で `claude -p --model claude-haiku-4-5-20251001 'Reply with the single word: ok' < /dev/null` の疎通を見てから stable に進む
5. 初回の手順 5 で `subject` / `kid` の記録を確かめる

### 初回の鋳造

1. **鍵を作って鍵束に追記する。** 秘密鍵は標準出力にだけ出るので、`>>` で鍵束に足す
   ```bash
   mkdir -p ~/.config/llm-gateway/keys
   llm-gateway auth keygen --kid claude-mbp-2026-09 >> ~/.config/llm-gateway/keys/claude.jwks.jsonl
   chmod 600 ~/.config/llm-gateway/keys/claude.jwks.jsonl
   ```
   期待結果: 鍵束に 1 行の JWK (`{"kty":"OKP","crv":"Ed25519","kid":"claude-mbp-2026-09","d":…,"x":…}`) が増える。`chmod 600` は新規作成時に要る (umask 次第で 644 で作られるため)

2. **token を鋳造する。**
   ```bash
   llm-gateway auth sign --key ~/.config/llm-gateway/keys/claude.jwks.jsonl --kid claude-mbp-2026-09 --sub kawaz-mbp --ttl 180d
   ```
   期待結果: 1 行の JWT。`--sub` は機械ごとに分けると、知らせ (`/llm-gateway/events` の `subject` / `kid`) で見分けられる。`--ttl` は `max_ttl` 以下

3. **クライアントに持たせる。** Claude Code なら `settings.json` の `env.ANTHROPIC_AUTH_TOKEN` に入れ、`ANTHROPIC_BASE_URL` を `…/ns-claude` にする

4. **設定に `keys_file` を書いて読み込ませる。** 設定を初めて変える時だけ restart が要る
   ```toml
   [ns.claude]
   auth = "jwt"
   keys_file = "~/.config/llm-gateway/keys/claude.jwks.jsonl"
   max_ttl = "400d"
   ```
   ```bash
   llm-gateway check --config <設定ファイル>
   llm-gateway daemon restart --all
   ```
   期待結果: `check` がエラーを出さず、restart が全 unit で healthz まで戻る

5. **確かめる。** 新しい Claude Code のセッションで 1 往復し、`/llm-gateway/events` の `request` に `subject` / `kid` が載ることを見る

### ローテ (既定 180 日)

token の ttl は鍵の周期 + 猶予にしておく。

1. **新しい kid の鍵を鍵束に追記する** (旧 kid の行は残す)。restart は要らない
   ```bash
   llm-gateway auth keygen --kid claude-mbp-2027-03 >> ~/.config/llm-gateway/keys/claude.jwks.jsonl
   ```
2. **新しい kid で各クライアントの token を鋳造し直して貼り替える** (初回の手順 2〜3、`--kid claude-mbp-2027-03`)。走行中の Claude Code のセッションは起動時の env を持ち続けるので、全セッションの再起動を待つ
3. **旧 kid の利用が 0 になったのを確かめる。** `/llm-gateway/events` で旧 kid の `kid` が来なくなったこと
4. **鍵束から旧 kid の行を削除する。** これが旧 kid の失効で、次のリクエストから反映される (restart 無し)。削除は一時ファイルに書いて `mv` で置き換える (rename なので gateway は書きかけを見ない。エディタで直接上書きすると、先頭の数行だけ書かれた途中を読んで他の kid を一時的に失いうる)

   ```bash
   ring=~/.config/llm-gateway/keys/claude.jwks.jsonl
   grep -v '"kid":"claude-mbp-2026-09"' "$ring" > "$ring.tmp" && chmod 600 "$ring.tmp" && mv "$ring.tmp" "$ring"
   ```

### 漏洩時

1. **漏れた kid の行を鍵束から即座に削除する** (ローテの手順 4 と同じく、一時ファイルに書いて `mv` で置き換える)。次のリクエストから、その kid で鋳造した token は全て通らなくなる。別の kid で鋳造した機械は影響を受けない
2. 必要なら、その機械のために新しい kid で初回の手順 1〜3 をやり直す (鍵束への追記と token の貼り替え。restart は要らない)
3. 鍵束ファイル自体が漏れた疑いがある時は全行が漏洩扱い。新しい kid の行を追記して全機械の token を鋳造し直し、古い行を全て削除する

## 失敗時の切り分け

| 症状 | 原因 | 対処 |
|---|---|---|
| クライアントが 401 (`WWW-Authenticate: Bearer error="invalid_token"`) | kid が鍵束に無い / 期限切れ / 寿命が `max_ttl` 超え / 署名が合わない / `iss` / `aud` 不一致 | 応答では区別しない。gateway のログの `refused a namespace token` の `reason` を見る (`unknown_kid` / `expired` / `ttl_exceeded` / `bad_signature` / `claims` / `alg_mismatch` / `malformed` 等) |
| 起動しない (設定エラー) | 起動時に鍵束が読めない (ファイルが無い・権限・不正な行・kid の重複) | エラーの行番号の行を直す。`keygen` の出力以外を書いていないか、同じ kid を 2 度追記していないかを見る |
| 鍵束を書き換えたのに反映されない (稼働中) | 読み直しに失敗し、前の鍵束で検証を続けている | gateway のログの警告を見て鍵束を直す。直せば次のリクエストで読み直す |
| 行を消したのに通る | mtime の粒度の内で 2 度書いて、2 度目を取りこぼした / 別の鍵束を編集している | 鍵束を `touch` し直す。設定の `keys_file` (`extends` の派生で差し替えていないか) が指すファイルを確かめる |
| 足した鍵で通らない | 別の鍵束に追記した / `sign` の `--kid` が違う | `llm-gateway auth jwks --key <鍵束>` で kid の一覧を見て、設定の `keys_file` と突き合わせる |

## 関連

- DR-0030 §5 / §6 (鍵束と ns 認証の方式、`jwt` の規定)
- `docs/design/ns-auth-jwt.md` (実装計画)
- MANUAL の「`jwt` namespaces」「`auth`」の節
