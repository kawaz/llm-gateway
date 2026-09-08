---
title: 単価表 gap warning が exclude で隠したモデルにも出る (ノイズ)
status: open
category: bug
created: 2026-09-08T13:59:42+09:00
last_read:
open_entered: 2026-09-08T13:59:42+09:00
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

# 単価表 gap warning が exclude で隠したモデルにも出る (ノイズ)

## 概要

2026-09-08 v0.43.3 起動時に `the price table does not describe this model`
gap=`gpt-reserve` / `gpt-5.5` / `gpt-5.4-mini` /
`claude-opus-4-5-20251101` / `claude-sonnet-4-5-20250929` の warning が出る。
これらは全て `[routes.*] exclude` / `[ns.*.filter] exclude` で隠しており、
どの namespace からも選択できないモデル。実害はないが起動ログのノイズ。

## 背景

gap 検査 (router.rs の catalog 更新時) が絞り込み前の catalog を対象にして
いるため、exclude 済みで到達不能なモデルにも warning が出てしまう。

方針案: gap 検査の対象を「いずれかの namespace から visible なモデル」に
限定する (`Namespace::allows` を通したあとの集合で判定)。

関連: DR-0026、DR-0014。

## 受け入れ条件

- [ ] exclude 済みで到達不能なモデルは gap warning の対象から外れる
- [ ] 実際に visible なモデルの gap は引き続き warning される

## TODO

<!-- wip 時のみ -->
