# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-09-12 | bug | open | [replay-stats-record-passes-millis-as-seconds](./2026-09-12-replay-stats-record-passes-millis-as-seconds.md) | replay の自送信が stats.record() にミリ秒を渡し日付が壊れる |
| 2026-09-11 | bug | open | [replay-store-dir-collides-with-signal-store](./2026-09-11-replay-store-dir-collides-with-signal-store.md) | replay store の置き場が合図方式の見張り置き場と衝突している |
| 2026-09-11 | bug | open | [daemon-status-version-stuck-null](./2026-09-11-daemon-status-version-stuck-null.md) | unit 起動直後の version 問い合わせ失敗が status に null のまま残る |
| 2026-09-10 | request | open | [supervisor-log-reopen-on-rotation](./2026-09-10-supervisor-log-reopen-on-rotation.md) | 監督者のログ fd をログローテーション後に開き直す |
| 2026-09-10 | bug | open | [supervisor-crash-leaves-orphan-children](./2026-09-10-supervisor-crash-leaves-orphan-children.md) | 監督者クラッシュ後に子プロセスが残り新監督者とポート衝突する |
| 2026-09-09 | request | open | [daemon-restart-order-and-grace](./2026-09-09-daemon-restart-order-and-grace.md) | daemon restart --all の順序と停止猶予を設定可能にする |
| 2026-09-09 | bug | open | [service-status-running-false-while-loaded](./2026-09-09-service-status-running-false-while-loaded.md) | service status の service.running が稼働中でも false になる |
| 2026-09-08 | request | open | [keepalive-daily-counters](./2026-09-08-keepalive-daily-counters.md) | keepalive ping の実発火回数を日別に永続化する |
| 2026-09-07 | request | open | [stats-per-session-and-subagent](./2026-09-07-stats-per-session-and-subagent.md) | stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望) |
| 2026-09-03 | tech-memo | open | [oauth-requires-claude-code-shape](./2026-09-03-oauth-requires-claude-code-shape.md) | サブスク OAuth 経路は Claude Code の形をしていない request を 429 "Error" で弾く (真因と対応候補) |
