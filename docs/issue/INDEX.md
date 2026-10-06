# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-10-04 | design | open | [service-reregister-env-drift-not-detected](./2026-10-04-service-reregister-env-drift-not-detected.md) | service register は XDG を plist に固定しているが、re-register で値が変わっても止まらず、client 側の検知も無い (hyoui からのフラグ) |
| 2026-09-25 | design | open | [low-priority-slow-requests-should-wait-not-switch](./2026-09-25-low-priority-slow-requests-should-wait-not-switch.md) | Claude Code の /low-priority (anthropic-usage-limit: slow) の 429/529 を DR-0009 の断りとして扱わず透過する |
| 2026-09-17 | design | open | [plan-command-routing-and-quota-forecast](./2026-09-17-plan-command-routing-and-quota-forecast.md) | plan command で routing 解決 + quota forecast を pull 型で出す |
| 2026-09-15 | design | open | [store-layer-for-replaceable-persistence](./2026-09-15-store-layer-for-replaceable-persistence.md) | 永続化の器を Store 層として責務で切り、file backend を差し替え可能にする |
| 2026-09-07 | request | open | [stats-per-session-and-subagent](./2026-09-07-stats-per-session-and-subagent.md) | stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望) |
| 2026-09-03 | tech-memo | open | [oauth-requires-claude-code-shape](./2026-09-03-oauth-requires-claude-code-shape.md) | サブスク OAuth 経路は Claude Code の形をしていない request を 429 "Error" で弾く (真因と対応候補) |
