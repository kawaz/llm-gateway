---
title: リクエストボディを全量メモリに載せてから転送している
status: resolved
category: design
created: 2026-07-29T00:30:40+09:00
last_read: 2026-07-29T17:26:43+09:00
open_entered: 2026-07-29T00:30:40+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-07-29T17:40:03+09:00
discard_reason:
pending_reason:
close_reason: ["dr/DR-0003","implemented","done:v0.4.0 (34959b96), comment-only change — crates/llm-gateway-server/src/lib.rs messages handler + crates/llm-gateway/src/backend/anthropic/forward.rs module comment. Verified necessary: route fallback (5xx/401/403/429/529) and beta resend (DR-0003) both require resending the same body; per-route model rewriting produces a different body per route, incompatible with a body readable only once (streaming)."]
blocked_by:
origin: 自リポ TODO
---

# リクエストボディを全量メモリに載せてから転送している

## 概要

`crates/llm-gateway-server/src/lib.rs` の `messages` ハンドラは
`axum::body::to_bytes(body, MAX_BODY)` でリクエストボディを全量読み切ってから
`serde_json::from_slice` で `Value` にパースし、`crates/llm-gateway/src/backend/anthropic/forward.rs`
の `forward::send` はその `Value` を `reqwest::RequestBuilder::json(&body)`
で再シリアライズして upstream に送る。応答側 (`forward::send` の
`resp.bytes_stream()`) はチャンクのままストリーム転送する設計になっているのに対し、
リクエスト側は非対称に「受信→パース→再構築→送信」という全量バッファ経路になっている。

## 背景

`provider.adapt(&mut body, &mut headers)` (model 名の書き換え等) が
JSON 構造への書き込みを必要とするため、現状は body 全体をパースしないと
adapt できない。これは意図的なトレードオフだが、大きな画像添付や長い会話履歴を
含むリクエストではボディサイズがそのままメモリに乗る (`MAX_BODY` 上限はあるが、
上限まではプロセスメモリを消費する)。ストリーミング応答側は「解釈せずバイト列の
まま中継できる」設計コメントがあるのに対し、リクエスト側は同じ理由付けが
書かれておらず、非対称である理由が記録に残っていない状態。

## 受け入れ条件

- [ ] リクエスト側を全量バッファする現状が `adapt` の要件上避けられないものか
      (= 部分パース・streaming JSON 書き換え等で回避できないか) を調査する
- [ ] 避けられない設計であれば、その理由を forward.rs 冒頭のコメントのように
      明示し (Design rationale)、応答側との非対称性を記録に残す
- [ ] 回避可能であれば、対応方針を決めて別 issue に切り出す
