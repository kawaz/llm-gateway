---
title: CLI に daemon / service サブコマンド体系を採用する
status: resolved
category: design
created: 2026-09-09T09:31:21+09:00
last_read:
open_entered: 2026-09-09T09:31:21+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-09T16:13:13+09:00
discard_reason:
pending_reason:
close_reason: ["dr/DR-0028","implemented","runbook/2026-09-09-migrate-launchd-to-service","done:v0.44.0〜v0.44.1で段階A/B/C実装、2026-09-09 16時台に稼働機を全断なくmigrate完了","derived:service-status-running-false-while-loaded"]
blocked_by:
origin: kawaz 提案 (2026-09-09)
---

# CLI に daemon / service サブコマンド体系を採用する

## 概要

CLI に `daemon` / `service` サブコマンド体系を採用する (kawaz 提案 2026-09-09、ccmsg v2 で作り直し中の体系を hyoui / cache-warden / llm-gateway に横展開)。

体系の正本は claude-rules-personal の参照知識カタログ `knowledge` skill の
`reference/cli-daemon-subcommands.md` (起票依頼済み、未作成) で、ccmsg v2 の
仕様確定後にこちらへ写す。

要旨:

- `llm-gateway daemon {run,supervise,add,remove,list,start,stop,restart,status,log}`
- `llm-gateway service {register,unregister,start,stop,status,log}`
- 引数なし/`--help` はテキスト help、他は JSON/JSONL
- unit = 登録単位 (llm-gateway では config ファイル = 11301/11302)
- 共通 `--all` / `--follow`

## 背景

llm-gateway 固有の決定事項:

1. unit の設定に `binary_path` (既定は自分自身。stable = brew、unstable = repo
   build の 2 系統運用を維持するため) — supervisor は exec するだけ
2. `restart --all` は rolling (Caddy が 11301 優先で 11302 に落ちる構成なので
   11302 → healthz 復帰 → 11301 の順、全断を作らない)
3. 既存 `llm-gateway status` (DR-0021 の upstream 障害状況) は `upstream status`
   に寄せる (`daemon status` との語彙衝突回避)
4. unit の既定値は「登録が 1 つならそれ、複数なら名前か `--all` を要求」
5. これで `config.toml` の「待ち受けないダミー (CLI の usage/stats 用)」が
   不要になる = 登録簿から listen を引く

関連: launchd plist `com.kawaz.llm-gateway-{stable,unstable}` を
`service register` に置き換える移行手順を runbook に書く。

## 受け入れ条件

- [ ] ccmsg v2 の daemon/service サブコマンド仕様確定後、
      `reference/cli-daemon-subcommands.md` の内容を踏まえて llm-gateway 側の
      設計を最終化する
- [ ] `daemon` / `service` サブコマンド群を実装し、既存 `status` を
      `upstream status` へ移行する
- [ ] launchd plist を `service register` ベースの運用に移行する runbook を書く
