# Runbook: ns 認証 `jwt` の鍵の鋳造・ローテ・失効

- Last Updated: 2026-10-13

## 適用ケース

- Claude Code などのクライアントに、`auth = "jwt"` の namespace を使わせ始める時 (初回の鋳造)
- 鍵の周期 (既定 180 日) が来た時 (ローテ)
- 秘密鍵か token が漏れた、または漏れた疑いがある時 (失効)
- Claude Code の `apiKeyHelper` や codex CLI の `auth.command` を `auth token` (gateway が入れ替える自動の鍵、DR-0037) に切り替える時

この runbook の鋳造・ローテ・失効は `keys_file` の**手の鍵**の手順。自動の鍵は各 unit の daemon が `key_rotation` (既定 7 日) ごとに作って入れ替えるので、人の手順は要らない (MANUAL の「自動の鍵」)。

## 前提

- `llm-gateway` の CLI が手元にあること (`auth keygen` / `auth jwks` / `auth sign`)
- 鍵束は ns ごとに 1 ファイル、1 行 1 JWK の jsonl (秘密鍵込み、600)。置き場の例は `~/.config/llm-gateway/keys/<ns>.jwks.jsonl`。ファイルが正本で、鍵の追加は行の追記、失効は行の削除。追加は `>>` の追記、削除は一時ファイルに書いて `mv` で置き換える (rename で原子的に)
- **鍵束の変更に restart は要らない**。gateway は検証のたびに鍵束の mtime を見て、変わっていれば読み直す (反映は次のリクエストから)。設定ファイル (`auth = "jwt"` / `keys_file` 等) を変えた時は `daemon reload` で読み直させる (restart は要らない。ns の認証は reload で変えられる欄)
- kid は **機械 (または用途) ごとに 1 本**。1 台分だけを失効できるようにするため
- 以下の例は ns が `claude`、鍵束が `~/.config/llm-gateway/keys/claude.jwks.jsonl`

## 手順

### `auth` 未設定 (Open) の ns を無停止で `jwt` に移す

認証方式は ns 単位で 1 つなので、`auth = "jwt"` を書いた瞬間、その ns を使う走行中セッションは token を持っていなければ 401 になる (手前の Caddy は 401 で fail over しない)。ns が Open (`auth` 未設定) のうちは gateway が `Authorization` を検査しないので、**先に鍵束を作ってクライアント全部へ JWT を配り、走行中セッションが入れ替わるのを待ってから設定を切り替える**。この「配布済みだが未検査」の期間が移行期間で、token と jwt を同時に受ける方式を gateway に足す必要はない。

1. 鍵束を作る (初回の手順 1)。**設定にはまだ書かない**。`keys_file` は `auth = "jwt"` と同時にしか書けず、片方だけだと `check` が落ちる
2. JWT を鋳造し、クライアントに貼る (初回の手順 2〜3)。Claude Code は `settings.json` の `env.ANTHROPIC_AUTH_TOKEN`、codex は `[model_providers.<id>]` の `http_headers = { Authorization = "Bearer <JWT>" }`。この時点で新旧どちらの値でも通る
3. その ns を使う走行中セッションが自然に再起動され切るのを待つ (急がない)。残っているものは `ccmsg peers` 等で数える
4. ns に `auth = "jwt"` / `keys_file` / `max_ttl` を書き、`check` → `llm-gateway daemon reload unstable` (初回の手順 4)。**貼った JWT が正しいかはここで初めて検査される**ので、unstable で `claude -p --model claude-haiku-4-5-20251001 'Reply with the single word: ok' < /dev/null` の疎通を見てから `llm-gateway daemon reload stable` に進む。unstable と stable が同じ設定ファイルを読んでいる場合、ファイルを書いた時点ではどちらも変わらず、reload した台から順に切り替わる
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

3. **クライアントに持たせる。** Claude Code なら `settings.json` の `env.ANTHROPIC_AUTH_TOKEN` に入れ、`ANTHROPIC_BASE_URL` を `…/ns-claude` にする。codex CLI なら `[model_providers.<id>]` の `http_headers = { Authorization = "Bearer <JWT>" }` に入れ、`base_url` を `…/ns-claude/v1` にする

4. **設定に `keys_file` を書いて読み直させる。** 鍵束と違って設定は見張られないので、書き終えたら `daemon reload` で伝える
   ```toml
   [ns.claude]
   auth = "jwt"
   keys_file = "~/.config/llm-gateway/keys/claude.jwks.jsonl"
   max_ttl = "400d"
   ```
   ```bash
   llm-gateway check --config <設定ファイル>
   llm-gateway daemon reload --all
   ```
   期待結果: `check` がエラーを出さず、reload の答えが全 unit で `"ok":true`。`ok: false` の unit は旧設定のまま走っているので、`error.message` を直して reload し直す

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

### helper を `auth token` に切り替える

前提: クライアントが届きうる全 unit (Caddy の fail over 先を含む) が、自動の鍵を読む版 (0.67.0 より新しい版) であること。古い版は自動の鍵の token を `unknown_kid` で断る。

1. **自動の鍵ができているのを確かめる。** unit を新しい版で起動 (または restart) すると作られる
   ```bash
   llm-gateway check --config <設定ファイル>
   ```
   期待結果: ns の行に `automatic keys <置き場> (every 7d)` と `automatic kids <ns>-<日付>-<4 桁>` が出る。`(every 7d, none yet; …)` ならまだ作られていない (新しい版の unit が起動していない)。`automatic keys <置き場>: cannot read (<理由>)` なら壊れていて入れ替えも止まっているので、理由の行を一時ファイル + `mv` で消す。

   置き場は設定の `[stats] dir` で決まり、書いていなければ設定を読むプロセスの `XDG_STATE_HOME` で決まる。`[stats] dir` を明示するか、`check` を Claude Code と同じ env で通し、unit のログ (`created an automatic key ... path=`) の置き場と同じかを確かめる
2. **発行を試す。** token の値は画面に出さず、長さと終了コードだけ見る
   ```bash
   llm-gateway auth token --config <設定ファイル> --ns claude --sub kawaz-mbp | wc -c
   ```
3. **クライアントの設定を書き換える。** `--kid <手の kid>` を足しておくと、自動の鍵が読めない間はその鍵に落ちる
   - Claude Code: `settings.json` の `apiKeyHelper` に上のコマンドを置き、`env.ANTHROPIC_AUTH_TOKEN` は空文字で上書きする (親プロセスの env に残った旧 token が helper より優先されるため)
   - codex CLI: `$CODEX_HOME/config.toml` の `[model_providers.<id>]` から `http_headers` の `Authorization` (と `env_key`) の行を消し、`[model_providers.<id>.auth]` の `command = "llm-gateway"`、`args` に上のコマンドの引数を並べて置く (MANUAL の「codex CLI の設定」)。`args` はシェルを通らないので `--config` は `~` を使わず絶対パスで書く
4. **走行中のセッションは起動時の token を持ち続ける**ので、全セッションの再起動を待つ。`/llm-gateway/events` の `kid` が自動の kid (`<ns>-<日付>-…`) に変わるのを見る
5. **旧 helper 用の手の kid の行を消す。** その kid が events に来なくなり、その kid で鋳造した token の `exp` (helper なら最長 `--ttl`) を過ぎてから、ローテの手順 4 と同じく一時ファイル + `mv` で消す

### 漏洩時

1. **漏れた kid の行を鍵束から即座に削除する** (ローテの手順 4 と同じく、一時ファイルに書いて `mv` で置き換える)。次のリクエストから、その kid で鋳造した token は全て通らなくなる。別の kid で鋳造した機械は影響を受けない
2. 必要なら、その機械のために新しい kid で初回の手順 1〜3 をやり直す (鍵束への追記と token の貼り替え。restart は要らない)
3. 鍵束ファイル自体が漏れた疑いがある時は全行が漏洩扱い。新しい kid の行を追記して全機械の token を鋳造し直し、古い行を全て削除する
4. **ns を消した・改名した時は、state の自動の鍵のファイル (`<[stats] dir>/jwt-keys/<旧 ns>.jwks.jsonl` と `.lock`) も消す。** daemon は消さず、同じ名前の `jwt` の ns を書き直すとその鍵がまた効く。全 unit の設定からその ns が消えたことを確かめてから消す (`check` が `warning: these automatic keys belong to no jwt namespace of this configuration` に挙げる)
5. 自動の鍵のファイル (`<[stats] dir>/jwt-keys/<ns>.jwks.jsonl`) が漏れた疑いがある時は、全 unit を止めてからファイルを消し、unit を起動し直す (起動時に新しい自動の鍵が作られる)。`auth token` を使うクライアントは次の呼び直しで新しい鍵の token を受け取る。止めずに消すと、走っている unit は前の束を保ったまま検証を続ける

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
