# llm-gateway

クライアントに認証を意識させない、薄い LLM proxy。

```
ANTHROPIC_BASE_URL=http://127.0.0.1:xxxx
     ↓
llm-gateway ── OAuth プール    (サブスク認証を解決)
            └─ Bedrock        (API key を解決)
```

## 何をするか

クライアント (Claude Code 等) は `ANTHROPIC_BASE_URL` を設定するだけ。
サブスク認証ヘッダも Bedrock のキーも gateway が解決してルーティングする。

実装するのは 3 つだけ:

1. **OAuth token リフレッシュ** — 8 時間で失効するので自動更新
2. **session → auth 固定** — `X-Claude-Code-Session-Id` をキーに貼り付け
3. **モデル名ルーティング** — 優先順位付き (Bedrock 優先、落ちたら OAuth 等)

## 何をしないか

**リクエストボディを一切触らない。ヘッダも足さない。**

前身の CLIProxyAPI は Claude Code の偽装 (beta フラグ注入 / cloak /
device profile) を行い、それが実障害の原因になった。ここでは素通しする。
サブスク token が偽装なしで通ることは実測済み (DR-0001)。

429 検知 → cooldown → failover も持たない (実測で発生 0 件)。
必要になってから足す。

## ステータス

**設計段階。実装はこれから。**

## ドキュメント

- [docs/decisions/INDEX.md](./docs/decisions/INDEX.md) — 判断記録 (DR)

背景となる実測・調査は **`kawaz/llm-notes`** (private) が正本:

- `docs/findings/2026-07-27-thin-proxy-poc.md` — 自作の成立条件の実測
- `docs/findings/2026-07-27-bedrock-api-key-integration.md` — Bedrock 経路
- `docs/decisions/DR-0002-thin-proxy-design.md` — 自作を決めた判断

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
