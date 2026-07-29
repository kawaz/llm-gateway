---
title: refresh の detached task が panic すると in_flight の acquire が永久待ちになる
status: open
category: bug
created: 2026-07-29T16:54:22+09:00
last_read:
open_entered: 2026-07-29T16:54:22+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: 自リポ TODO
---

# refresh の detached task が panic すると in_flight の acquire が永久待ちになる

## 概要

credential store の refresh を detached task 化した実装
(`crates/llm-gateway/src/credential/store.rs` の `refresh_once` →
`tokio::spawn`) で、spawn 内の `do_refresh` が panic すると in_flight の除去と
broadcast 送信が走らず、その credential への以後の acquire が `recv()` で永久待ちに
なる。in_flight 側に Sender clone が残るため channel も閉じない。

## 背景

verify-fix2-3 レビュー (2026-07-29) で指摘。旧実装 (リーダー実行型) にも同型の穴が
あり、cancellation 安全化 (commit lowtqtmm) の回帰ではない。`do_refresh` 経路に
unwrap は無く現実的リスクは低いため低優先。

対処案: spawn 内を Drop guard で in_flight remove するか、AbortHandle 管理に切り替える。

## 受け入れ条件

- [ ] `do_refresh` が panic した場合でも in_flight エントリが除去され、待機中の
      acquire が (成功 or エラーとして) 復帰することを確認するテストがある
- [ ] 対処方針 (Drop guard か AbortHandle 管理か) を選定し実装する
