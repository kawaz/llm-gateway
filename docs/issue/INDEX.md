# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-09-23 | bug | open | [codex-cli-traffic-recorded-as-unknown-origin](./2026-09-23-codex-cli-traffic-recorded-as-unknown-origin.md) | codex CLI 経由のトラフィックが origin `unknown` で記録され `codex` にならない |
| 2026-09-23 | bug | open | [error-config-reused-for-non-config-failures](./2026-09-23-error-config-reused-for-non-config-failures.md) | Error::Config が設定読み込み以外の失敗 (SSE 読み取り・model 欠落・応答中断) に流用されている |
| 2026-09-17 | design | open | [plan-command-routing-and-quota-forecast](./2026-09-17-plan-command-routing-and-quota-forecast.md) | plan command で routing 解決 + quota forecast を pull 型で出す |
| 2026-09-15 | design | open | [store-layer-for-replaceable-persistence](./2026-09-15-store-layer-for-replaceable-persistence.md) | 永続化の器を Store 層として責務で切り、file backend を差し替え可能にする |
| 2026-09-14 | task | open | [keepalive-absence-tests-can-pass-vacuously](./2026-09-14-keepalive-absence-tests-can-pass-vacuously.md) | keepalive の「何も送られない」テストが判断回数を検証せず vacuous に pass しうる |
| 2026-09-10 | request | open | [supervisor-log-reopen-on-rotation](./2026-09-10-supervisor-log-reopen-on-rotation.md) | 監督者のログ fd をログローテーション後に開き直す |
| 2026-09-10 | bug | open | [supervisor-crash-leaves-orphan-children](./2026-09-10-supervisor-crash-leaves-orphan-children.md) | 監督者クラッシュ後に子プロセスが残り新監督者とポート衝突する |
| 2026-09-09 | request | open | [daemon-restart-order-and-grace](./2026-09-09-daemon-restart-order-and-grace.md) | daemon restart --all の順序と停止猶予を設定可能にする |
| 2026-09-07 | request | open | [stats-per-session-and-subagent](./2026-09-07-stats-per-session-and-subagent.md) | stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望) |
| 2026-09-03 | tech-memo | open | [oauth-requires-claude-code-shape](./2026-09-03-oauth-requires-claude-code-shape.md) | サブスク OAuth 経路は Claude Code の形をしていない request を 429 "Error" で弾く (真因と対応候補) |
