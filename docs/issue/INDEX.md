# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-09-09 | bug | open | [keepalive-foreign-standby-fires-immediately](./2026-09-09-keepalive-foreign-standby-fires-immediately.md) | v0.43.6 で keepalive が自己ループする (ping → 返信 → classifier → 即 ping、6 秒周期) |
| 2026-09-08 | design | open | [keepalive-via-messaging-socket](./2026-09-08-keepalive-via-messaging-socket.md) | keepalive の合図を ccmsg 経由でなく messaging socket へ直接注入する |
| 2026-09-08 | request | open | [keepalive-daily-counters](./2026-09-08-keepalive-daily-counters.md) | keepalive ping の実発火回数を日別に永続化する |
| 2026-09-07 | request | open | [stats-per-session-and-subagent](./2026-09-07-stats-per-session-and-subagent.md) | stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望) |
| 2026-09-03 | tech-memo | open | [oauth-requires-claude-code-shape](./2026-09-03-oauth-requires-claude-code-shape.md) | サブスク OAuth 経路は Claude Code の形をしていない request を 429 "Error" で弾く (真因と対応候補) |
