---
title: launchd label を規約 (com.github.kawaz.<repo>) に揃える (jp.kawaz.llm-gateway.supervise → com.github.kawaz.llm-gateway.supervise)
status: open
category: task
created: 2026-10-04T17:30:00+09:00
last_read: 2026-10-04T17:30:00+09:00
open_entered: 2026-10-04T17:30:00+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
---

# launchd label を規約 (com.github.kawaz.<repo>) に揃える

kawaz 製ツールの launchd label / systemd unit の命名規約が reference `cli-daemon-subcommands` の「OS 登録の名前」節に入った (2026-10-04、hyoui での議論から)。要点: 逆引き DNS は所有を確かめた名前空間だけを使う、既定は `com.github.kawaz.<repo>[.<sub>...]`、先頭はリポ名、systemd の user unit も同じ文字列に `.service`。

## 現状

`jp.kawaz.llm-gateway.supervise` (`jp.kawaz` は根拠の記録なく使われ始め (DR-0028)、hyoui の DR-0034 がそれを手本に広げた。ドメインごとの使い分けの規約が無い。旧 label `com.kawaz.llm-gateway` (`.disabled`) も残っており、`kawaz.com` は kawaz の所有ではない。)

## やること

- label を `com.github.kawaz.llm-gateway.supervise` にする (採否と最終形は本リポで判断)
- 移行: 旧 label を unregister (bootout + plist 削除) → 新 label で register。plist を直接置いている場合はその手順を runbook に
