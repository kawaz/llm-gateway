---
title: codex の枠照会 API が黙って失敗し usage が unobserved のまま (spend_down が効かない)
status: resolved
category: bug
created: 2026-09-05T21:41:27+09:00
last_read:
open_entered: 2026-09-05T21:41:27+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-06T15:16:47+09:00
discard_reason:
pending_reason:
close_reason: ["done:v0.38.2 で refresh 経路 (probe_usage) に preset.apply_quota を接続し snapshot に反映、ask_limits 失敗時の warn ログも追加。実機検証: 再起動直後 unobserved -> usage?refresh=true で codex 2件とも observed (7d 窓 reset 付き)。真因は API 失敗ではなく apply_quota 未接続だった"]
blocked_by:
origin: 自リポ TODO
---

# codex の枠照会 API が黙って失敗し usage が unobserved のまま (spend_down が効かない)

## 概要

`GET /llm-gateway/usage?refresh=true` を叩いても codex credential (codex-kawaz / codex-emrd) は `support: unobserved` のままで、`secondary` (7d 相当) の reset / window が取れない。実リクエストの応答ヘッダ (`x-codex-secondary-*`) からは取れるので、gpt リクエストが 1 本通るまで観測できない。

## 背景

DR-0018 §4 (リセット時刻か窓長が取れない credential は昇格しない) により、`gpt-*` 規則の `spend_down_within` が再起動直後〜最初の gpt リクエストまで効かない。2026-09-05 に personal ns の gpt-* 規則へ `spend_down_within = "25%"` を足した際に発覚。

観測 (2026-09-05 12:38 UTC、v0.38.0):

- 再起動直後に `usage?refresh=true` → claude 3 件は observed、codex 2 件は unobserved
- ログに codex の枠照会に関する warn/error は出ていない
- `gateway.rs` の `ask_limits` が `api.fetch(...).await.ok()` でエラーを捨てているため、失敗理由が分からない (WhamUsage の URL / 認証 / 応答形式のどれで落ちているか未切り分け)

## 受け入れ条件

- [ ] `ask_limits` の失敗を warn 1 行で出す (credential 名 + 理由)
- [ ] codex の枠照会が失敗する原因を特定し直す (API 側の変更 / chatgpt-account-id / 応答形式)
- [ ] `usage?refresh=true` で codex の secondary 窓が observed になる
- [ ] 起動直後でも spend_down が gpt-* に効くことを route_names で確認
