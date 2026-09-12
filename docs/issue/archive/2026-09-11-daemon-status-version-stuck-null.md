---
title: unit起動直後のversion問い合わせ失敗がstatusにnullのまま残る
status: resolved
category: bug
created: 2026-09-11T00:01:42+09:00
last_read:
open_entered: 2026-09-11T00:01:42+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-12T20:32:43+09:00
discard_reason:
pending_reason:
close_reason: ["done: 原因は稼働監督者がv0.44.1のまま(status要求ごとに本人へ問い合わせる実装a2f6de27が未展開)。監督者をservice stop/startで0.46.1にした直後からdaemon status --all/versionが両unitのrunning versionを返すことを実機確認。regression testはsupervisor.rsの既存テストが覆う"]
blocked_by:
origin: 自リポ TODO
---

# unit起動直後のversion問い合わせ失敗がstatusにnullのまま残る

## 概要

`daemon status` / `version` コマンドの unit `version` (running) が、unit 起動直後に
問い合わせて失敗すると null のまま更新されない。

## 背景

実測 2026-09-11 00:00 JST (v0.46.0 展開の `daemon restart --all` 直後):
`curl /llm-gateway/version` は両 unit とも `{"version":"0.46.0"}` を返すのに、
`daemon status --all` と `version` は数分後も `running: null`。

以前の restart では数秒後の status で取れていたので、監督者が子の起動直後
(listen 前) に 1 回だけ問い合わせて結果を保持している、または healthz 復帰前の
問い合わせ失敗を最終値にしていると推定 (未確認、`supervisor.rs` の status
応答生成箇所を確認)。

方針: status 要求のたびに問い合わせる (2s timeout はそのまま)、または healthz
成功後に問い合わせる。

関連: DR-0028 決定 9。

## 受け入れ条件

- [ ] `daemon restart --all` 直後でも、`daemon status --all` / `version` が
      実際の子プロセスの version を反映する (null で固着しない)
- [ ] `supervisor.rs` の status 応答生成箇所で、問い合わせタイミング
      (起動直後 1 回のみ / healthz 復帰前後) を確認し原因を特定する

## TODO

<!-- wip 時のみ -->
