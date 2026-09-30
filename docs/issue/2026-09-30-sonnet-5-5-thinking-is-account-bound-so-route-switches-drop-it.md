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

前提として route → account の対応 (linked account 含む) を設定で表せる必要がある。

## 実測 2026-09-30 (research doc 参照)

別 account (`claude-emrd` で生成 → `claude-kawazzz`) への再送は 200 で thinking が黙って落ちる。`input_tokens` が同一 account の対照より thinking_tokens 分少ない。beta `thinking-binding-controls-2026-08-01` を付けても `input_transformations` は `[]` で、理由は報告されない。同一 account の履歴改変では `prefix_binding_mismatch` が出るので header 自体は効いている。

よって beta header で drop を観測する方式は成立しない。gateway が自前で判断する。

## 適用範囲の前提 (2026-09-30 一次資料再確認)

束縛の種類とモデルごとの現状:

| 束縛 | Sonnet 5.5 | Fable 5.1 | Opus 5.5 |
|---|---|---|---|
| account 束縛 | あり | なし | なし |
| prefix 束縛 | あり | あり | あり |
| model 束縛 | あり | あり | あり |

- account 束縛は現時点で Sonnet 5.5 だけ。prefix 束縛と model 束縛は全モデル・全 account で効く
- ただし資料は「newer Claude models の性質」「later checks add values」と書いており、他モデルに広がらない保証は無い
- Fable の routing は Bedrock を fail over 先に含み、Bedrock は別 account 扱い。Fable に広がると、統括セッションの長い thinking 連鎖が fail over の瞬間に黙って消える (致命的、kawaz 指摘)
- 実測どおり beta header は account 不一致を報告しないので、gateway が自前で判断する必要がある

設計方針: 対策をモデル固有にしない。**モデルごとの束縛ポリシーを設定で持ち、既定リストは今は `claude-sonnet-5-5` だけ**にする。他モデルに広がったら設定 1 行で追加できる形にする。

## 採る方向 (kawaz 裁定 2026-09-30)

失われたことを記録するだけの案 (観測・検知のみ、単純に剥がす等) は推論を救えないので却下。次の 3 点セットで行く。

- (a) **session ごとに開始 account をロックする**。affinity を「優先」でなく「固定」にする。ロックした account の枠切れ時の扱い (待つ / 切替) は issue `low-priority-slow-requests-should-wait-not-switch` の裁定と合流する
- (b) **やむなく切替える時は cache 破壊前提で thinking を text に変換する**。切替点 (session の message index) を記録し、それより前の thinking block を通常の text block に書き換える。先頭に `THINKING:\n` を付けて webui で区別できるようにする。変換は決定的で、切替後に新 account が生成した block は素通しする
- (c) **持ち越せるのは client が持つ本文だけ**。`thinking_display = "summarized"` の ns では要約しか渡らない。`redacted_thinking` は変換不能

## 裁定点

- thinking_display を full に戻すか、要約持ち越しで割り切るか

DR 起草は裁定後。

## 受け入れ条件

- [ ] 上記裁定点が裁定され、DR が起草される
- [ ] 実機 2-turn マトリクスの検証結果が docs/research に記録される
- [ ] session の開始 account ロックと、切替時の thinking → text 変換が実装される
- [ ] route → account (linked 含む) の対応を設定で表せる

## TODO

<!-- wip 時のみ -->
