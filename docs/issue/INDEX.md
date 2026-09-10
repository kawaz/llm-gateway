# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-09-10 | request | open | [supervisor-log-reopen-on-rotation](./2026-09-10-supervisor-log-reopen-on-rotation.md) | 監督者のログ fd をログローテーション後に開き直す |
| 2026-09-10 | task | open | [ecosystem-review-2026-09](./2026-09-10-ecosystem-review-2026-09.md) | エコシステム外部レビュー (2026-09) の指摘への対応検討 |
| 2026-09-10 | bug | open | [supervisor-crash-leaves-orphan-children](./2026-09-10-supervisor-crash-leaves-orphan-children.md) | 監督者クラッシュ後に子プロセスが残り新監督者とポート衝突する |
| 2026-09-09 | request | open | [daemon-restart-order-and-grace](./2026-09-09-daemon-restart-order-and-grace.md) | daemon restart --all の順序と停止猶予を設定可能にする |
| 2026-09-09 | bug | open | [service-status-running-false-while-loaded](./2026-09-09-service-status-running-false-while-loaded.md) | service status の service.running が稼働中でも false になる |
| 2026-09-08 | design | open | [keepalive-via-messaging-socket](./2026-09-08-keepalive-via-messaging-socket.md) | keepalive の合図を ccmsg 経由でなく messaging socket へ直接注入する |
| 2026-09-08 | request | open | [keepalive-daily-counters](./2026-09-08-keepalive-daily-counters.md) | keepalive ping の実発火回数を日別に永続化する |
| 2026-09-07 | request | open | [stats-per-session-and-subagent](./2026-09-07-stats-per-session-and-subagent.md) | stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望) |
| 2026-09-03 | tech-memo | open | [oauth-requires-claude-code-shape](./2026-09-03-oauth-requires-claude-code-shape.md) | サブスク OAuth 経路は Claude Code の形をしていない request を 429 "Error" で弾く (真因と対応候補) |
