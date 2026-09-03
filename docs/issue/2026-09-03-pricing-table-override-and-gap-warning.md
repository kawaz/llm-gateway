---
title: 内蔵単価表の config 上書きと catalog-pricing ギャップ warning
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

# 内蔵単価表の config 上書きと catalog-pricing ギャップ warning

## 概要

内蔵単価表 (`preset/pricing.rs`) を config で上書き/追加できるようにする。
あわせて、catalog に存在するが単価行が無い、または glob マッチで旧世代の単価に
誤って飲まれてしまうモデルを `check` と discovery 時に warning する。

## 背景

Fable 5.1 の cache read 単価 (通常単価の 0.025 倍) が `claude-fable-5-*` の glob
パターンに飲まれ、専用の単価行が無かったため黙って $1.0/MTok (旧世代相当) で
計算されていた。2026-09-02 に発見・修正済みだが、根本原因である「新モデルの
単価行が無い/glob に誤って飲まれる」構造自体は残っている。

Anthropic 側に機械可読な価格 API は無いため、単価表の自動取得は行わない前提。
かわりに:

- 内蔵単価表を config で override / 追加できるようにする (ハードコード修正待ちにしない)
- catalog にモデルはあるが専用単価行が無い/glob 一致で疑わしいケースを
  `check` コマンドと discovery 時に warning
- 単価表の「確認日」を status 出力に出し、古さを可視化する案も検討 (自動更新は
  できないので、人間が定期的に確認するきっかけを作る)

## 受け入れ条件

- [ ] config から内蔵単価表を上書き/追加できる
- [ ] catalog に存在し単価行が無い (or glob 一致のみで専用行が無い) モデルを
      `check` 実行時に warning できる
- [ ] discovery 時にも同様の warning が出る
- [ ] 単価表の確認日を status 出力等で可視化する方式を決めて実装 (要検討: 手動
      更新日をどこに保持するか)

## TODO

<!-- wip 時のみ -->
