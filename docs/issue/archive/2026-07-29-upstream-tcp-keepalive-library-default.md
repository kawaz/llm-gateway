---
title: upstream 接続の TCP keepalive をライブラリ既定任せにしている
status: resolved
category: design
created: 2026-07-29T00:14:52+09:00
last_read: 2026-07-29T17:26:43+09:00
open_entered: 2026-07-29T00:14:52+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-07-29T17:40:03+09:00
discard_reason:
pending_reason:
close_reason: ["done:v0.4.0 (f5d03c09) — crates/llm-gateway/src/gateway.rs Design rationale comment; tcp_keepalive 20s + interval 10s + retries 3, h2 PING 20s/timeout 10s + while_idle explicit. Long-idle real-world behavior not empirically verified (stated as unverified in the comment)."]
blocked_by:
origin: 自リポ TODO
---

# upstream 接続の TCP keepalive をライブラリ既定任せにしている

## 概要

`crates/llm-gateway/src/gateway.rs` の `reqwest::Client::builder()` は
`connect_timeout` のみ明示指定しており、TCP keepalive は reqwest/hyper の
ライブラリ既定 (未設定=無効) のままになっている。upstream への接続は
ストリーミング応答で長時間張りっぱなしになる前提のため、経路上の
NAT/LB がアイドルコネクションを黙って切断した場合に、appl 層が気づくのが
実際にデータを流そうとした時点まで遅れる可能性がある。

## 背景

`gateway.rs` のコメントにある通り「upstream の応答は長い。生成が続く限り
待つ必要があるので、全体のタイムアウトは置かない」という設計判断が既にある。
この設計は「長時間コネクションが健全である」ことを前提にしているが、
健全性そのものを検知する仕組み (TCP keepalive) は組み込まれていない。

`reqwest::ClientBuilder` には `tcp_keepalive(Duration)` (send 側の
keepalive probe 間隔) があり、必要なら `tcp_keepalive_interval` /
`tcp_keepalive_retries` も設定できる。ライブラリ既定に任せる選択が
意図的なものか、単に見落としなのかが記録に残っていない状態。

## 受け入れ条件

- [ ] 既定任せのままで問題ないか (upstream 側/中間 LB の挙動、実運用での
      切断検知タイミング) を調査し、意図的な判断として記録に残す
- [ ] 対応が必要と判断した場合、`tcp_keepalive` 系の値を明示設定し、
      設計意図をコード上の comment (Design rationale) に残す
