---
title: codex CLI 経由のトラフィックが origin `unknown` で記録され `codex` にならない
status: resolved
category: bug
created: 2026-09-23T19:02:54+09:00
last_read:
open_entered: 2026-09-23T19:02:54+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-24T17:16:38+09:00
discard_reason:
pending_reason:
close_reason: ["dr/DR-0024","dr/DR-0025","dr/DR-0029","implemented"]
blocked_by:
origin: 自リポ TODO
---

# codex CLI 経由のトラフィックが origin `unknown` で記録され `codex` にならない

## 概要

DR-0025 は「Responses 形式で受けた 1 本は origin `codex`」と定めるが、2026-09-23 の stats (`~/.local/state/llm-gateway/stats/2026-09-23.127-0-0-1-11301.json`) では codex CLI (Claude Code の Agent tool 経由の `worker-sol-high` 等、`codex exec`) の gpt-6-sol 91 本 / gpt-5.6-sol 203 本が `unknown` に積まれている。同日に curl で `/ns-personal/v1/responses` へ直接送った 2 本は `codex` になっている。

## 背景

`llm-gateway stats --by origin --days 1 --unit unstable` の抜粋 (2026-09-23):

```
  codex   chatgpt-kawaz gpt-6-sol     2   ...   0.0661
  unknown chatgpt-kawaz gpt-5.6-sol 203   ...  80.9720
  unknown chatgpt-kawaz gpt-6-sol    91   ...  13.1175
```

推測 (未裏取り): codex CLI (0.155 系) が `/v1/responses` の POST でなく別の入口 (WebSocket の Responses、compaction 用 endpoint、`/v1/chat/completions` 等) を使っていて、その経路では `RequestOrigin::Codex` が付かない。または tailnet URL (Caddy) 経由で入る path が違う。裏取りしてから採否を決めてほしい。

## 受け入れ条件

- [ ] codex CLI が実際に使っている入口 (path / プロトコル) を events か access log で確認する
- [ ] その入口でも Responses 形式なら origin を `codex` にする (DR-0025 の意図どおり)
- [ ] stats の `--by origin` で codex CLI のトラフィックが `codex` に積まれる
