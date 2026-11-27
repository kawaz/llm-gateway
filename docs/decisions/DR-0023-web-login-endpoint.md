# DR-0023: Web 経由の OAuth 再認証口 (`/llm-gateway/login`)

- Status: Active
- Date: 2026-08-28

## 文脈

refresh token が失効すると再ログインが要るが、現行の `llm-gateway login` (CLI) はブラウザと gateway ホスト上のターミナルの両方を要する。redirect_uri が `http://localhost:54545/callback` (client_id に登録済みの値) 固定のため、リモートのブラウザで認可すると callback が利用者側の localhost に着地して gateway に届かない。tailnet 越しにリモートから再認証を完結したい (kawaz 依頼 2026-08-27、docs/issue/2026-08-27-web-login-endpoint.md)。

## 決定

gateway の HTTP 口に web login フローを足す。**claude_oauth のみ**。localhost callback の代わりに **手動コード貼り付けフロー** を使う: redirect_uri を `https://console.anthropic.com/oauth/code/callback` にすると認可後に console がコード (`code#state` 形式) を画面表示するので、利用者がそれを gateway のページへ貼り付ける。callback がどこにも「飛ばない」ためリモートで完結する。client_id は Claude Code と同一 (このフローの実績元)。

### エンドポイント

- `GET /llm-gateway/login` — config の credential を列挙する HTML。claude_oauth は credential 専用ページへのログインリンク、codex_oauth は CLI の案内を表示
- `GET /llm-gateway/login/{name}/start` — state/verifier を生成してメモリに保持 (TTL 10 分、単回使用) し、credential 名、別タブで開く authorize URL、手順、コード貼り付けフォームを含む HTML を返す
- `POST /llm-gateway/login/{name}` — 貼り付けられた `code#state` を受け、state でセッションを引き当て、code 交換 → verify → 保存。credential 名を含む結果 HTML を返す

### 保存

CLI login と同じ経路: `store.lock(id)` で締め出し (DR-0010) → 既存を土台に `tokens.to_stored` → `store.store`。常駐 refresh との消し合いを防ぐ。

### 再ログインのアカウント照合

既に保存されている認証情報に再ログインするとき、今回承認されたアカウントが保存済みのアカウントと違えば保存しない。ブラウザで別のアカウント (別のプロファイルにログイン中のもの等) を承認したまま上書きすると、2 つの認証情報が同じアカウントを指す状態に黙って落ち、どちらの枠を使っているのか分からなくなるため。

- 照合は `tokens.to_stored` で、ロックの内側で読み直した既存の内容に対して行う。Web と CLI はどちらもここを通る。refresh はこの関数を通らないので、照合の対象外
- 比べるのは保存済みの欄だけ: `email` (両 type)、`codex_oauth` はさらに `account_id`。比べるのは保存済みの側と今回の側の両方に値がある欄だけで、どちらかが空・無しならその欄では断らない (email を持たない古いファイルや、応答に email が無い場合を弾かないため)。email は ASCII の大文字小文字を区別せず比べ、account_id は完全一致で比べる
- 保存済みのファイルが無い名前への初回ログインは比べる相手が無いので通る
- 断るときはファイルに一切書かず、エラー (`could not complete authorization: ...`) に保存済みと今回のアカウント (email、または account_id) と対処を載せる。Web では失敗のページに出る。token は載せない
- アカウントを意図して替えたいときは、その認証情報のファイル (`<store>/<name>.json`) を消してからログインし直す。上書きを強制するオプションは持たない。替える操作は頻繁に起きるものではなく、ファイルを消す手順で足りる。強制のオプションがあると、取り違えたときにもそれを付けて押し通す道ができ、照合の意味が薄れる

### 対象外 (codex_oauth)

OpenAI 側にコード表示ページの公知の実績が無く、redirect_uri は Codex CLI 登録の localhost:1455 のみ。web ページでは「CLI (`llm-gateway login --type codex_oauth
<name>`) をローカルで実行」と案内する。

## 境界 (認証)

追加の認証は置かない。namespace 口と同じく境界は tailnet/Caddy (DR-0006 と同じスタンス)。tap の loopback 限定は適用しない — リモートから使うことが目的のため。credential を書き換える口だが、書けるのは「正規の認可を通った token」だけで、攻撃者が任意値を書き込める口ではない。state (CSRF) + PKCE + 単回使用 TTL で認可の横取りを防ぐ。

## リスク

- redirect_uri は client_id に登録済みの値に限定される。gateway 自身の URL では `Redirect URI https://…/llm-gateway/login/<name>/callback is not supported by client.` と認可画面で拒否される。Web login は登録済みの Anthropic console redirect_uri に固定する
- 交換時の redirect_uri は認可時と一致が必須。web フローは AuthProfile の redirect_uri を console 変種に差し替えて交換まで通す

## 不採用案

- **gateway 自身の URL を redirect_uri にする**: client_id に未登録の値は認可時点で弾かれる。実機でも `is not supported by client` と拒否され、自前 client_id の登録手段が無い
- **CLI を SSH で叩く運用のみ**: PC が手元に無い場面 (スマホ + tailnet) を救えない
