---
title: count_tokens 経路で model alias が解決されない疑い
status: open
category: bug
created: 2026-09-03T18:29:37+09:00
last_read: 2026-09-04T15:30:07+09:00
open_entered: 2026-09-03T18:29:37+09:00
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

# count_tokens 経路で model alias が解決されない疑い

## 概要

`POST /ns-personal/v1/messages/count_tokens` に model `"haiku"` (alias) を渡すと
`no route configured for model haiku` で失敗する。フル ID
`claude-haiku-4-5-20251001` を渡せば成功する。一方 `/v1/messages` 本経路では
alias がそのまま解決されて動く。count_tokens 経路だけ alias 解決が効いていない
疑いがある。

## 背景

実測 (2026-09-03):

- `POST /ns-personal/v1/messages/count_tokens` + `model: "haiku"` →
  `no route configured for model haiku`
- 同じリクエストを `model: "claude-haiku-4-5-20251001"` (フル ID) に変えると成功
- `/v1/messages` 本経路は alias のままで route が解決される (= alias 解決自体は
  どこかに実装されている)

forward の resolve 処理が count_tokens 経路に組み込まれていない、または
routing の照合が alias 解決前の生の model 名を見ている可能性がある。

## 受け入れ条件

- [ ] count_tokens 経路のコードを読み、alias 解決がどこで (されるべきなのに)
      スキップされているか特定する
- [ ] `/v1/messages` 本経路の alias 解決処理との実装差分を確認する
- [ ] 修正し、`model: "haiku"` で count_tokens が成功することを実機確認する
- [ ] 他の alias (sonnet 等) でも同様に動作することを確認する
