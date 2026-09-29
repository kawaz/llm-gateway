---
title: Sonnet 5.5 の thinking は account 束縛なので route 切替で黙って消える
status: open
category: design
created: 2026-09-30T07:42:25+09:00
last_read:
open_entered: 2026-09-30T07:42:25+09:00
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

# Sonnet 5.5 の thinking は account 束縛なので route 切替で黙って消える

## 概要

Sonnet 5.5 の thinking は account 束縛なので route 切替で黙って消える。切替に慎重になるオプションの検討。調査は `docs/research/2026-09-30-preserved-thinking-and-account-switching.md`。

一次資料: Sonnet 5.5 が生成した thinking block は生成 account (または linked account) でしか効かず、別 account が送ると API は block を捨てた上で 200 を返す (beta `thinking-binding-controls-2026-08-01` を送れば `input_transformations` に `thinking_dropped` / `organization_binding_mismatch` が出る)。

## 背景

gateway は DR-0009 の fail over (401/403/429/529/5xx) と pace_cap / denial で別 credential に移り、affinity は route 名単位で account の同一性を知らない。そのため Sonnet 5.5 の会話は切替後に推論の連続性を黙って失う。

候補:

1. 同一 account 優先 + 切替時に beta header を付けて drop を観測し、stats / events に出す (推奨、可用性維持)
2. 同一 account への hard pin (枠不足で会話停止)
3. thinking を剥がして切替
4. 切替を拒否して client に返す

前提として route → account の対応 (linked account 含む) を設定で表せる必要がある。実機の 2-turn マトリクス (同 key / 別 key 同 account / 別 account / linked、beta 有無) は未検証。

kawaz 裁定待ち: どの候補で行くか、実機検証を許可する account 2 つ。

## 実測 2026-09-30 (research doc 参照)

別 account (`claude-emrd` で生成 → `claude-kawazzz`) への再送は 200 で thinking が黙って落ちる。`input_tokens` が同一 account の対照より thinking_tokens 分少ない。beta `thinking-binding-controls-2026-08-01` を付けても `input_transformations` は `[]` で、理由は報告されない。同一 account の履歴改変では `prefix_binding_mismatch` が出るので header 自体は効いている。

よって候補 (1) の「header で drop を観測」は成立しない。候補を見直す:

- (1') 同一 account 優先 + 切替時に thinking を含む会話は切替を拒否する (client に 429/503 と retry-after を返す)
- (2') 切替時に thinking block を gateway が剥がして送る (どのみち落ちるので明示的に落として挙動を読めるようにする、events に記録)
- (3') `input_tokens` の期待値との差で事後検知して events に `thinking_dropped_suspected` を出す (検知のみ)

裁定点: (1')/(2')/(3') の組み合わせ、route → account 対応を設定で表す形。

## 受け入れ条件

- [ ] 候補 (1)〜(4) のどれで行くか裁定される
- [ ] 実機 2-turn マトリクスの検証結果が docs/research に記録される
- [ ] route → account (linked 含む) の対応を設定で表せる

## TODO

<!-- wip 時のみ -->
