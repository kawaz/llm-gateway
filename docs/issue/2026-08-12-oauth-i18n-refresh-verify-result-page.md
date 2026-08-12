---
title: oauth ログインフローの日本語文言残り (DR-0008 続き)
status: open
category: task
created: 2026-08-12T12:01:39+09:00
last_read:
open_entered: 2026-08-12T12:01:39+09:00
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

# oauth ログインフローの日本語文言残り (DR-0008 続き)

## 概要

`crates/llm-gateway/src/credential/oauth.rs` の `refresh_failure_reason` /
`exchange_failure_reason` / `verify_failure_reason` / `result_page` (ブラウザ表示
HTML 文言) 等のヘルパー関数群に日本語文言が残っている。

## 背景

2026-08-12 の core crate 英語化で、自己完結して訳せる 4 箇所は英語化済み。
残るこの塊は文言相互依存とテストが絡むためまとめて対応が必要で、後回しに
なっていた。DR-0008 の「触ったついでに直す」運用方針で対応する。

`result_page` はブラウザに表示する HTML であり、他の error message (プロセス側
ログ / API レスポンス) とは性質が異なる。英語化するか日本語 UI として残すか
(= DR-0008 の対象範囲かどうか) の判断が必要。

## 受け入れ条件

- [ ] `refresh_failure_reason` / `exchange_failure_reason` / `verify_failure_reason`
      の日本語文言が英語化されている
- [ ] `result_page` を DR-0008 の対象に含めるか判断し、含める場合は英語化する
      (含めない場合はその理由を記録する)
- [ ] 関連テストが文言変更に追従している

## TODO

<!-- wip 時のみ -->
