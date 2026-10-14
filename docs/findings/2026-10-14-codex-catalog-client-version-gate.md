# codex catalog は client_version で絞られ minimal_client_version だけが gate ではない

- Date: 2026-10-14

## 判明した事実

- codex CLI 0.162.0 から gateway (ns-personal) 経由で `gpt-6.1-sol` を呼ぶと `404 no route configured for model gpt-6.1-sol` になる。gateway の `/v1/models` にも 6.1 が無い
- ChatGPT Codex backend `GET https://chatgpt.com/backend-api/codex/models?client_version=<v>` が返す一覧は、`client_version` が 0.159.0 未満だと `gpt-6.1-sol` を含まない
- `gpt-6.1-sol` 自身の `minimal_client_version` は `0.153.0` で、0.159.0 未満でも満たしている。upstream の一覧の絞り込みは各モデルの `minimal_client_version` だけではない
- 対処として `crates/llm-gateway/src/discovery.rs` の `CODEX_CLIENT_VERSION` を `0.155.0` から `0.162.0` (手元の公開済み codex CLI の版) に上げた

## 実用的な示唆 / ベストプラクティス

- 新モデルが catalog に出ない時は、`client_version` を変えて実測し、出る最小の公開済み版 (か手元の版) に `CODEX_CLIENT_VERSION` を更新する。`minimal_client_version` は目安にすぎない (DR-0026 の更新手順はこの趣旨)
- クライアント中継 (DR-0025 §7) は codex CLI が名乗る版をそのまま渡すので影響を受けない。影響するのは定期 refresh (`discovery::fetch`) だけ

## 検証の詳細

### 版別の catalog 件数 (同一 credential、`chatgpt-account-id` ヘッダ付き)

| client_version | 件数 | gpt-6.1-sol |
|---|---|---|
| 0.155.0 / 0.156.0 / 0.157.0 / 0.158.0 | 9 | 無し |
| 0.159.0 / 0.160.0 / 0.161.0 / 0.162.0 / 0.170.0 | 10 | 有り |

考察: 境界は 0.158.0 と 0.159.0 の間にある。増えるのは `gpt-6.1-sol` の 1 件。

### gpt-6.1-sol の記述

| 項目 | 値 |
|---|---|
| `minimal_client_version` | `0.153.0` |
| `visibility` | `list` |
| `context_window` | `500000` |
| `default_reasoning_level` | `low` |
| `supported_reasoning_levels` | low / medium / high / xhigh / max / ultra |

考察: `ultra` は新設 ("Maximum reasoning with automatic task delegation")。API 側の 6.1 Sol は context 1.05M で、Codex 経路の 500k とは別物 (`docs/research/2026-09-30-gpt-6-1.md`)。
