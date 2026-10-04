---
title: service register は XDG を plist に固定しているが、re-register で値が変わっても止まらず、client 側の検知も無い
status: open
category: design
created: 2026-10-04T14:00:00+09:00
last_read: 2026-10-04T14:00:00+09:00
open_entered: 2026-10-04T14:00:00+09:00
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

# service register は XDG を plist に固定しているが、re-register で値が変わっても止まらず、client 側の検知も無い

hyoui 側で web service を reference `cli-daemon-subcommands` と突き合わせた際に、先行例として本リポを確認して気づいた点 (2026-10-04)。フラグと一次資料の提示まで。採否と直し方は本リポで裏取りしてから決めてほしい。

## 観測

- `jp.kawaz.llm-gateway.supervise.plist` の `EnvironmentVariables` は `XDG_CONFIG_HOME` / `XDG_STATE_HOME` の 2 つ (`crates/llm-gateway-cli/src/service.rs` 146 行付近で register 時の値を書く)。固定そのものは reference どおり
- reference の同じ節は加えて「**re-register 時に、既存 unit の値と今の env が違えば差分を示して止まる。`--force` で上書き**」「**client 側にも検知を持つ** (daemon が名乗る場所と client の導出結果を比べ、違えば警告)」としている。`service.rs` / `service/platform.rs` を `force` / `differ` / `drift` 等で grep した範囲では、どちらも見当たらなかった (読解のみ、実行での確認はしていない)

## 参考

- reference `cli-daemon-subcommands` の「`service register` は場所を決める env を unit に固定し、変わったら止まる」節
- 同じ観点の issue が kawaz/ccmsg にある (`docs/issue/2026-09-30-service-pins-path-env-and-refuses-drift.md`)。ccmsg は `PATH` / `HOME` / `CCMSG_*_DIR` / XDG を固定しているが、差分で止まる処理は同じく未実装
- 固定する変数の一覧は path 導出コードが読む変数を正とする (reference)。本リポで `HOME` 等も導出に効くなら対象に入る
