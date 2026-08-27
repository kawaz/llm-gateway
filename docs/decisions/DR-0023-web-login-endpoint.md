# DR-0023: Web 経由の OAuth 再認証口 (`/llm-gateway/login`)

- Status: Accepted
- Date: 2026-08-28

## 文脈

refresh token が失効すると再ログインが要るが、現行の `llm-gateway login` (CLI) は
ブラウザと gateway ホスト上のターミナルの両方を要する。redirect_uri が
`http://localhost:54545/callback` (client_id に登録済みの値) 固定のため、
リモートのブラウザで認可すると callback が利用者側の localhost に着地して
gateway に届かない。tailnet 越しにリモートから再認証を完結したい (kawaz 依頼
2026-08-27、docs/issue/2026-08-27-web-login-endpoint.md)。

## 決定

gateway の HTTP 口に web login フローを足す。**claude_oauth のみ**。
localhost callback の代わりに **手動コード貼り付けフロー** を使う:
redirect_uri を `https://console.anthropic.com/oauth/code/callback` にすると
認可後に console がコード (`code#state` 形式) を画面表示するので、利用者が
それを gateway のページへ貼り付ける。callback がどこにも「飛ばない」ため
リモートで完結する。client_id は Claude Code と同一 (このフローの実績元)。

### エンドポイント

- `GET /llm-gateway/login` — config の claude_oauth credential を列挙する HTML。
  各行に開始リンクと、コード貼り付けフォーム
- `GET /llm-gateway/login/{name}/start` — state/verifier を生成してメモリに保持
  (TTL 10 分、単回使用) し、authorize URL へ 302
- `POST /llm-gateway/login/{name}` — 貼り付けられた `code#state` を受け、state で
  セッションを引き当て、code 交換 → verify → 保存。結果を HTML で返す

### 保存

CLI login と同じ経路: `store.lock(id)` で締め出し (DR-0010) → 既存を土台に
`tokens.to_stored` → `store.store`。常駐 refresh との消し合いを防ぐ。

### 対象外 (codex_oauth)

OpenAI 側にコード表示ページの公知の実績が無く、redirect_uri は Codex CLI 登録の
localhost:1455 のみ。web ページでは「CLI (`llm-gateway login --type codex_oauth
<name>`) をローカルで実行」と案内する。

## 境界 (認証)

追加の認証は置かない。namespace 口と同じく境界は tailnet/Caddy (DR-0006 と同じ
スタンス)。tap の loopback 限定は適用しない — リモートから使うことが目的のため。
credential を書き換える口だが、書けるのは「正規の認可を通った token」だけで、
攻撃者が任意値を書き込める口ではない。state (CSRF) + PKCE + 単回使用 TTL で
認可の横取りを防ぐ。

## リスク

- **console callback の受理は未検証** (authorize が この client_id +
  `https://console.anthropic.com/oauth/code/callback` を受けるか、コード表示形式が
  `code#state` か)。Claude Code 本体が同 client_id で使うフローなので受理見込みは
  高いが、初回の実機ログイン (claude-kawazzz の再認証) で確定させる。弾かれたら
  本 DR を Superseded にして撤退判断に戻る
- 交換時の redirect_uri は認可時と一致が必須。web フローは AuthProfile の
  redirect_uri を console 変種に差し替えて交換まで通す

## 不採用案

- **gateway 自身の URL を redirect_uri にする**: client_id に未登録の値は認可時点で
  弾かれる。自前 client_id の登録手段が無い
- **CLI を SSH で叩く運用のみ**: PC が手元に無い場面 (スマホ + tailnet) を救えない
