---
title: codex 経路が画像を含む tool_result を拒否し route 全滅で 503 になる
status: wip
category: bug
created: 2026-09-19T17:30:25+09:00
last_read: 2026-09-23T18:11:18+09:00
open_entered: 2026-09-19T17:30:25+09:00
wip_entered: 2026-09-23T18:14:19+09:00
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: ccmsg
---

# codex 経路が画像を含む tool_result を拒否し route 全滅で 503 になる

## 概要

ccmsg webui のセッションから Agent tool で起動した codex worker (agent 定義 worker-sol-high、model gpt-5.6-sol) が、Read tool で PNG (kawaz が ccmsg に貼った実機スクショ) を読んだ直後に落ちた。

エラー文言:

```
API Error: 503 all routes for model gpt-5.6-sol failed: codex-emrd: could not read the configuration: tool_result may only contain text blocks.
```

## 背景

2026-09-19 08:29 JST に観測。画像を含む tool_result を codex 経路に流すと route が全滅して 503 になる。同じ画像を claude 系 worker では読めた (= codex 系固有の制約)。

ワークアラウンド: 画像を読む作業は claude 系 worker に振る。

この issue はフラグのみ。画像 tool_result を codex 経路で text に落とすか、非対応であることを worker 側に明示的なエラーで返すかは gateway 側で裏取りしてから決めてほしい (= 実装方針の判断は当事者に委ねる)。

## 受け入れ条件

- [ ] codex 経路 (gpt-5.6-sol 等) が画像を含む tool_result を受け取った場合の実機挙動を裏取り
- [ ] 画像非対応を「route 全滅 503」でなく明示的なエラーとして返す、または text へのフォールバックを行う対応方針を決定
- [ ] 決定した対応を実装

## TODO

<!-- wip 時のみ -->

原因分析済み: 拒否は gateway 変換層 (preset/openai/request.rs tool_result_text) で、Error::Config 型の使い回しと Switch::to_next による route 切替が 503 の原因。A (変換失敗を 400 で返す、route 切替も transport 障害計上もしない) を実装着手。画像の扱い (B プレースホルダ / C input_image 転送) は上流の実機裏取り後に決める。
