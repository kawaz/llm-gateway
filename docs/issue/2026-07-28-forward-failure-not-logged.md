---
title: 転送の失敗を記録できない (ストリーム中継の失敗が無記録)
status: open
category: bug
created: 2026-07-28T22:58:19+09:00
last_read:
open_entered: 2026-07-28T22:58:19+09:00
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

# 転送の失敗を記録できない (ストリーム中継の失敗が無記録)

## 概要

SSE ストリーム中継中に発生した失敗 (upstream 切断・タイムアウト・クライアント切断) が、どこにもログとして記録されない。

## 背景

- `crates/llm-gateway/src/gateway.rs:112` の `info!(... "転送しました")` は、upstream の**ヘッダを受け取った時点**で出る。`crates/llm-gateway/src/backend/anthropic/forward.rs` のコメント (37-40 行目付近) が「返るのは upstream のヘッダを受け取った時点。本文はまだ流れていない」と明記している。
- 本文 (ストリーム) は `crates/llm-gateway-server/src/lib.rs:116` で `Body::from_stream(upstream.body)` に渡され、そのままクライアントへ素通しする。
- **渡した後は gateway のコードを一切通らない**。したがって SSE 中継中の upstream 切断・タイムアウト・クライアント切断は、どこにも記録されない。
- 結果として「転送しました status=200」というログは「ヘッダまで届いた」しか意味せず、その転送が最後まで成功したことを保証しない。文言が実態とずれている。
- なお「成功時にしかログを出さない」という理解は不正確。upstream が 4xx/5xx を返し、経路を切り替えないケースでも status 付きで `info!("転送しました")` に出る。記録が欠けているのは**ボディ中継中の失敗**に限られる。

### 実害 (観測済み)

2026-07-28 に 3 セッションで `API Error: Response stalled mid-stream` が発生した際、gateway が原因かを切り分けられなかった。ストリーム中断がログに一切残らないため。詳細はセッション状態ファイル `/Users/kawaz/.cache/claude-session-state/llm-gateway/20260728-2131.md` の §9 参照。

## 対処の方向性 (案、未裁定)

- 中継するストリームを wrap し、終端 (正常完了 / エラー / 中断) と転送バイト数・所要時間を記録する
- リクエスト単位の ID を振り、開始ログと終了ログを対にする
- 現行 `"転送しました"` の文言を実態 (= ヘッダを受け取った) に合わせて改める

## 受け入れ条件

- [ ] SSE ストリーム中継中の失敗 (upstream 切断・タイムアウト・クライアント切断) が何らかの形でログに記録される
- [ ] ログ文言が「ヘッダ受信時点」であることと「本文まで転送完了」であることを区別できる
