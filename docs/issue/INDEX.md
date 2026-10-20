# Issue Index

| date | category | status | issue | 概要 |
|---|---|---|---|---|
| 2026-10-21 | request | open | [request-dump-with-timeout](./2026-10-21-request-dump-with-timeout.md) | リクエスト本文の一時ダンプ。外部から時限で on/off、古いものは自動で片付け |
| 2026-10-21 | design | open | [otel-observability-for-dev-environment](./2026-10-21-otel-observability-for-dev-environment.md) | 手元の開発全般で使う OpenTelemetry 観測基盤の検討 (Claude Code の classifier 理由から着手) |
| 2026-10-04 | design | open | [service-reregister-env-drift-not-detected](./2026-10-04-service-reregister-env-drift-not-detected.md) | service register は XDG を plist に固定しているが、re-register で値が変わっても止まらず、client 側の検知も無い (hyoui からのフラグ) |
| 2026-09-17 | design | open | [plan-command-routing-and-quota-forecast](./2026-09-17-plan-command-routing-and-quota-forecast.md) | plan command で routing 解決 + quota forecast を pull 型で出す |
| 2026-09-15 | design | open | [store-layer-for-replaceable-persistence](./2026-09-15-store-layer-for-replaceable-persistence.md) | 永続化の器を Store 層として責務で切り、file backend を差し替え可能にする |
| 2026-09-07 | request | open | [stats-per-session-and-subagent](./2026-09-07-stats-per-session-and-subagent.md) | stats / event で subagent 別・effort 別のコストを分離できるようにする (ccmsg からの要望) |
