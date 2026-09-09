---
title: service status の service.running が稼働中でも false になる
status: open
category: bug
created: 2026-09-09T16:12:51+09:00
last_read:
open_entered: 2026-09-09T16:12:51+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: 自リポ TODO
---

# service status の service.running が稼働中でも false になる

## 概要

`llm-gateway service status` の `service.running` が、監督者が動いていて `pid` も取れているのに `false` を返す。

## 背景

実測 2026-09-09 (v0.44.1、移行直後):

```json
{"registered":true,"running":true,"pid":85124,"service":{"loaded":true,"running":false,"pid":85124,"last_exit":"(never exited)"}}
```

トップレベルの `running` (socket 到達確認) は `true` で正しい。ネストした `service.running` だけが `false` になっており、`launchctl print gui/<uid>/<label>` の出力から `state = running` を読み取る箇所の解析ずれと推定 (未確認)。`launchctl print` の実出力と突き合わせて直す必要がある。

## 受け入れ条件

- [ ] `launchctl print gui/<uid>/<label>` の実出力を確認し、`service.running` を導出しているパース箇所を特定する
- [ ] 稼働中の launchd job で `service.running` が `true` になるよう修正する
- [ ] 修正後、実機で `llm-gateway service status` を叩いて `service.running: true` を確認する
