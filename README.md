# llm-gateway

クライアントに認証を意識させない、薄い LLM proxy。

```
ANTHROPIC_BASE_URL=http://127.0.0.1:xxxx
     ↓
llm-gateway ── OAuth プール    (Claude サブスク認証を解決)
            ├─ Bedrock        (API key を解決)
            └─ OpenAI 系       (ChatGPT サブスク認証を解決)
```

## 何をするか

クライアント (Claude Code 等) は `ANTHROPIC_BASE_URL` を設定するだけ。
サブスク認証ヘッダも Bedrock のキーも gateway が解決してルーティングする。
`gpt-*` を選んだ時も、裏で OpenAI 系のエンドポイントに繋ぐ。

構成要素は 4 つ:

1. **エンドポイントアダプタ** — Anthropic Messages API を話す口
2. **モデルバックエンドルータ** — モデル名から認証情報と upstream を優先順位で選ぶ
3. **バックエンドアダプタ** — upstream ごとの差分を吸収する
4. **クレデンシャルストア** — token の取得とリフレッシュ。永続化はプラガブル

## 何をしないか

**介入は最小限に留める。** ボディはルーティングキーである `model` フィールドだけ
書き換え、それ以外には触らない。ヘッダは認証情報の生成と、upstream が拒否する
`anthropic-beta` フラグの除去のみ。

前身の CLIProxyAPI は Claude Code の偽装 (beta フラグ注入 / cloak /
device profile) を行い、それが実障害の原因になった。Anthropic 経路では
サブスク token が偽装なしで通ることを実測済み (DR-0001)。

429 検知 → cooldown → failover も持たない (実測で発生 0 件)。
必要になってから足す。

## ステータス

**稼働中。** 転送 (claude / codex / Bedrock)・運用観測 (usage / stats / status / tap)・Web 再認証まで実装済み (詳細は [docs/MANUAL-ja.md](./docs/MANUAL-ja.md))。

## ドキュメント

- [docs/MANUAL-ja.md](./docs/MANUAL-ja.md) — HTTP API / CLI リファレンス
- [docs/decisions/INDEX.md](./docs/decisions/INDEX.md) — 判断記録 (DR)
- [docs/QUESTIONS.md](./docs/QUESTIONS.md) — 裁定・確認待ち

背景となる実測・調査は **`kawaz/llm-notes`** (private) が正本:

- `docs/findings/2026-07-27-thin-proxy-poc.md` — 自作の成立条件の実測
- `docs/findings/2026-07-27-bedrock-api-key-integration.md` — Bedrock 経路
- `docs/decisions/DR-0002-thin-proxy-design.md` — 自作を決めた判断

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
