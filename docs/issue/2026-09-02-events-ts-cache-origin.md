---
title: events の ts を upstream 送出開始時刻に寄せる + probe max_tokens=0 化
status: open
category: bug
created: 2026-09-02T13:37:08+09:00
last_read:
open_entered: 2026-09-02T13:37:08+09:00
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

# events の ts を upstream 送出開始時刻に寄せる + probe max_tokens=0 化

## 概要

`events` の `ts` を、現状の「upstream 応答ヘッダ受信時刻」から「upstream への送出開始時刻」に変更する。あわせて usage probe / 疎通プローブの `max_tokens` を `1` から `0` に変更する。

## 背景

prompt cache TTL の起点は「リクエスト開始時点」であり、応答ヘッダ受信時点ではない (公式 prompt-caching doc で確認済み、2026-09-02)。現状の実装は upstream からのヘッダ受信時刻を `ts` として記録しているため、streaming が長くなるほど ccmsg のリング表示上の TTL が実際より楽観側 (= 余裕がある側) にずれる。

また、公式が prewarm 用途で `max_tokens: 0` を正式化しており、出力トークンは非課金になる。現状の usage probe / 疎通プローブは `max_tokens: 1` を使っているが、これを `0` に変更することでコストを削減できる可能性がある。ただし、小さい probe だけが 429 になるという既知問題への効果は未検証。

## 受け入れ条件

- [ ] `events` の `ts` が upstream への送出開始時刻を記録するよう変更されている
- [ ] usage probe / 疎通プローブの `max_tokens` が `0` に変更されている
- [ ] `max_tokens: 0` 化が既知の 429 問題に与える影響を検証 (改善 / 無変化 / 悪化のいずれかを記録)
