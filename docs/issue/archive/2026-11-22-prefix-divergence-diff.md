---
title: 同じ prefix のはずの 2 本の request が、どこで食い違ったかを CLI で見せる
status: resolved
category: request
created: 2026-11-22T22:30:00+09:00
origin: ccmsg 統括経由の kawaz 依頼 (2026-11-22、btw の fork の cache 外れ)
close_reason: implemented
---

# 同じ prefix のはずの 2 本の request が、どこで食い違ったかを CLI で見せる

## 概要

ダンプ (DR-0038) の 2 本の request の本文、またはダンプ 1 本と keepalive の控え (DR-0027) を比べ、`system` / `tools` / `messages` のどの位置で食い違ったかを全部並べる CLI を足す。本文の中身は出さず、位置・種類・大きさ・hash を出す。

所見の正本は [docs/findings/2026-11-22-fork-prefix-divergence-gateway-options.md](../../findings/2026-11-22-fork-prefix-divergence-gateway-options.md)。

## 背景

`claude -p --resume <会話> --fork-session` の fork が本体の prompt cache を読めず、会話の分を全部書き直す件で、原因 (fork が transcript から組み立てた messages が本体の送った messages と 5 か所で違う) を特定するのに、記録用サーバと transcript の行の手作業の突き合わせが要った。

prompt cache は breakpoint の位置にしか entry を書かず、読む側は 20 位置しか遡らないので、違いが 1 か所でも残れば会話の分は全部外れる。直し方を決めるには、最初の 1 か所でなく**全部の違い**が要る。

## やること

- 2 つの本文を受けて比べる。比べる前に除くもの: `cache_control`、top-level の `model`、文字列の content と text block 1 つの違い
- 出すもの: 違う位置ごとに 1 行 (区分 `system` / `tools` / `messages`、message の番号と role、block の番号、違いの種類 = block の数 / block の type / 欄の値、両側の大きさと hash)。最後に「先頭からどこまで一致したか」
- 入力: ダンプの `*.request.body` のパス 2 つ、またはダンプの 1 本と控え (会話の id で引く)。受けた形 (ダンプ) と送った形 (控え) を比べる時は、ダンプの `request_rewrites` に載った書き換えが違いとして出ることを出力に書く
- 本文は出さない (控えの本文を見せる口を増やさない、DR-0042 決定 1 の線)

## 決めること

- サブコマンドの名前と置き場 (`dump` の下か、別の名前か)。CLI の設計は reference の `cli-design/preferences` に沿う
- 控えを入力に取る時の引き方 (会話の id に複数の系列がある時)

## 受け入れ条件

- 今回の 5 か所の違い (tool_use の `input` の形 2 か所、text block の数、ToolSearch の結果の block の数、role:system の message の `output_config`) と同じ形の合成本文で、5 か所全部が並ぶ試験がある
- 違いの無い 2 本で「全部一致」と出る

## 結果

`llm-gateway dump diff <a> <b>` / `llm-gateway dump diff --kept <session_id> [--prefix <prefix>] [--unit <name>] <b>` を足した (`crates/llm-gateway-cli/src/dump/diff.rs`、help の `dump` の段、MANUAL の `dump diff` の節)。

- **名前と置き場**: `dump` の下に置いた。入力がダンプの本文で、置き場の説明も `dump` の help にあるため。台には聞かず、手元のファイル (と `--kept` の時は控えの置き場) だけを読む
- **控えの引き方**: 会話の id で引き、系列が複数あれば `prefix_required` で断って候補 (`prefix` / `ns` / `model`) を返し、`--prefix` で選ばせる。置き場は `--unit` の台の `[stats]` (兄弟の台は共有)。控えを `<a>` (先に送られた側) に置く
- **出力**: JSON Lines。入力の行 2 行 (`form` が `received` / `sent`、ダンプの側は最後の試行の `request_rewrites` を添える)、違いの行が 1 か所 1 行、要約の行 1 行 (`first_difference` と `matched_prefix`、受けた形と送った形を混ぜた時は `note`)。違いの種類は `count` / `role` / `block_count` / `block_type` / `fields` / `value`
- やることに足したもの: 鍵の順だけの違いも出す (cache は bytes で比べる)。`tools` / `system` / `messages` 以外の top-level の欄は `params` として並べ、prefix の位置には数えない。block の数が違う時は、text をつなげて比べた結果 (`text_joined`) で割り方だけの違いか中身の違いかを分ける。文字列の違いには先頭と末尾の一致の長さを添える
- completion: このリポの CLI は completion 定義を持たない (DR-0038)。help と実装の突き合わせは試験で見る
- 試験: 5 か所の違いを持つ合成の親と fork で 5 か所全部が並ぶこと、違いの無い 2 本 (cache_control と model だけ違う形を含む) で `identical: true`、全部の出力行に本文の文字列が出ないこと
- 実物で比べた結果と、A に向けて分かったことは [所見の追記](../../findings/2026-11-22-fork-prefix-divergence-gateway-options.md#dump-diff-での実測) に書いた

## 関連

- [align-declared-fork-to-parent-prefix](../2026-11-22-align-declared-fork-to-parent-prefix.md) (この CLI で範囲の決め方を確かめてから着手する)
- DR-0038 (ダンプ)、DR-0027 (控え)、DR-0042 (本文を見せる口)
