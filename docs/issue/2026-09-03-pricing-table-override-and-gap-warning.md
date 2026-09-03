---
title: catalog-pricing ギャップ warning
status: open
category: design
created: 2026-09-03T12:07:36+09:00
last_read:
open_entered: 2026-09-03T12:07:36+09:00
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

# catalog-pricing ギャップ warning

## 概要

catalog に存在するが単価行が無い、または glob マッチで旧世代の単価に
誤って飲まれてしまうモデルを `check` と discovery 時に warning する。

## 裁定 (2026-09-03, kawaz)

単価表の更新はリリースごとのハードコードで良く、config での上書き/追加機能は
不要 → スコープから除外。残す対応は「catalog に存在するが単価行が無い、または
glob で旧世代の単価に飲まれるモデルを `check` と discovery で warning する」のみ。
加えて単価表の確認日を status 出力に出す案は任意 (実装するかは着手時に判断)。

## 背景

Fable 5.1 の cache read 単価 (通常単価の 0.025 倍) が `claude-fable-5-*` の glob
パターンに飲まれ、専用の単価行が無かったため黙って $1.0/MTok (旧世代相当) で
計算されていた。2026-09-02 に発見・修正済みだが、根本原因である「新モデルの
単価行が無い/glob に誤って飲まれる」構造自体は残っている。

Anthropic 側に機械可読な価格 API は無いため、単価表の自動取得は行わない前提。

## 受け入れ条件

- [ ] catalog に存在し単価行が無い (or glob 一致のみで専用行が無い) モデルを
      `check` 実行時に warning できる
- [ ] discovery 時にも同様の warning が出る
- [ ] (任意) 単価表の確認日を status 出力等で可視化する

## TODO

<!-- wip 時のみ -->
