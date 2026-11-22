---
title: 親を名乗った fork の messages を、親の控えの bytes に揃えて prompt cache を当てる
status: open
category: design
created: 2026-11-22T22:30:00+09:00
origin: ccmsg 統括経由の kawaz 依頼 (2026-11-22、btw の fork の cache 外れ)
---

# 親を名乗った fork の messages を、親の控えの bytes に揃えて prompt cache を当てる

## 概要

`claude -p --resume <親> --fork-session` の fork が、ヘッダで親の会話の id を名乗ってきたら、gateway は親の keepalive の控え (DR-0027) を引き、fork の messages のうち控えと同じ並びの範囲を控えの bytes に丸ごと置き換えてから送る。fork は親の書いた prompt cache を読めるようになる。

所見の正本は [docs/findings/2026-11-22-fork-prefix-divergence-gateway-options.md](../findings/2026-11-22-fork-prefix-divergence-gateway-options.md)。

## 背景

- Claude Code の `--resume` は transcript から履歴を組み立てるが、親が実際に送った形を再現しない message がある (tool_use の `input` の形、turn の途中の入力の合わせ方、ToolSearch の結果の block)。1 か所でも違うと、fork は会話の分を全部書き直す (約 55 万 token の会話で、btw 1 回ごとに約 53 万の write)
- 親の送った bytes を持っているのは gateway の控えだけで、fork を起動する側 (ccmsg) は Claude Code の送る本文を変えられない
- 揃えると、親の thinking block が署名された prefix に戻るので、署名との整合は fork の今の形より良くなる

## 着手前に決めること (DR)

- DR-0024 の禁則 (messages の中身に触らない) に、この場合だけの例外を書く
- 名乗りのヘッダの名前。ccmsg は fork の起動の `--settings` の `env.ANTHROPIC_CUSTOM_HEADERS` で載せられる (fork なしの `claude -p` で実測、fork ありは未確認)
- 引き当ての条件: 控えの `ns` が request の ns と同じ、解決後の model が同じ、`system` と `tools` が `cache_control` を除いて同じ
- 置き換える範囲の条件: 控えの messages の数以内で、各位置の role と tool_use / tool_result の id の並びが一致する。id の一致は「その履歴を既に持っている client だけが揃えられる」ことの担保も兼ねる
- 揃えきれない時は何もしない (範囲の途中で止めると、揃えなかった位置より後ろの thinking が prefix check に外れる。prefix check を強制する model では 400)
- role:system の message (effort の印を運ぶ) を置き換えるか残すか
- 範囲の末尾の 1 件 (控えの後に届いた入力が fork 側で合わさっていうる) の扱い
- 置き換える時は `cache_control` を持ち込まず、fork の breakpoint の位置を保つ (DR-0024「ブレークポイントは増やさない・動かさない」)
- 記録: ダンプの `request_rewrites` に置き換えた範囲を載せる。log にも 1 行

## 効かない場面 (DR に書く)

- 控えが無い会話: main の戦略が `keepalive` でない ns、8MB を超える会話 (`BODY_LIMIT`)、直前の 1 本が cache に乗らなかった会話

## 受け入れ条件

- 合成の親と fork (5 か所の違いを持つ) で、置き換え後の messages が範囲内で控えと一致し、範囲外は fork のまま
- 条件のどれかが外れる組み合わせで本文が素通しになる
- 実機 (unstable、使い捨てのセッション) で、名乗った fork が親の会話の分を cache_read で読む

## 関連

- [prefix-divergence-diff](./2026-11-22-prefix-divergence-diff.md) (先にやる。その CLI で実物の fork 3 件以上を比べ、範囲の決め方を確かめてから DR を起こす)
- DR-0024 (cache の方針と禁則)、DR-0027 (控え)、DR-0035 (thinking の変換。控えは変換後の形なので、比べる段を揃える)
- `docs/findings/2026-11-10-fable-5-2-preserved-thinking-vs-gateway-rewrites.md`
