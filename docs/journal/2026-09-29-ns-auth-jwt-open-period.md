# ns 認証 jwt への移行: Open 期間方式で鍵を鋳造

- 日付: 2026-09-29
- 関連: [runbook](../runbooks/ns-auth-jwt-rotation.md) の「`auth` 未設定 (Open) の ns を無停止で `jwt` に移す」、DR-0030 §6

## 裁定 (kawaz 2026-09-29)

JW-C1 の「切替タイミング a/b」は前提が違っていた。稼働 config の `[ns.personal]` `[ns.bare]` `[ns.emrd]` は `auth` 未設定 (Open) で、gateway は token 検査をしていない。よって JWT を先に全クライアントへ配れば、走行中セッション (古い値) と再起動後のセッション (JWT) が**両方通る期間**が自然に取れる。kawaz: 「それで良いです。open 期間を置く」。

## やったこと

- 対象 ns は personal / bare / emrd の 3 つ (zunsystem 面は gateway 経由でない)。codex は ns-personal を認証ヘッダなしで使っているので、切替前に `http_headers = { Authorization = "Bearer <JWT>" }` を持たせる (codex config reference で `model_providers.<id>.http_headers` が静的ヘッダと確認)
- ns ごとに鍵 1 組 (kid `<ns>-2026-09`): `~/.config/llm-gateway/private/<kid>.jwk`、公開鍵断片 `<ns>.keys.toml`。dotfiles の `.gitignore` に `/config/llm-gateway/private` を追加
- `keys` は `auth = "jwt"` と同時にしか書けない (`crates/llm-gateway/src/config.rs` の「`keys` / `max_ttl` / `iss` / `aud` belong to auth = "jwt"」) ので、config には切替まで何も入れない

## ハマり

- JWT を `settings.json` / codex config に書き込む操作は Claude Code の auto モード分類器が「Secret-Store Writes」で拒否する。この 1 手は kawaz が手元で実行する (`auth sign` の出力を jq で `.env.ANTHROPIC_AUTH_TOKEN` に、codex は `base_url` 行の下に `http_headers`)

## 残り

1. (kawaz 手動) JWT の鋳造と貼り付け: sub は `claude-personal` / `claude-bare` / `claude-emrd` / `codex-personal`、ttl 180d
2. 走行中セッションの入れ替わりを待つ
3. ns ごとに `auth = "jwt"` + `max_ttl = "400d"` + `<ns>.keys.toml` の中身を config に足し、unstable → stable で restart、疎通と `subject` / `kid` の記録を確認
