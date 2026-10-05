# tracing の単一 Dispatch 近道で thread-local ログ捕捉が空振りする

- Date: 2026-10-05

## 判明した事実

- `just test` (cargo llvm-cov 経由) で `exchange::tests::each_request_gets_its_own_number` が失敗する。1 本目の `marker` ログに span `exchange{req=..}` が付かず `left: 1 right: 2` になる (両方欠ける `left: 0` もある)。計測なしの `cargo test -p llm-gateway --lib` では 15/15 通過
- 再現頻度: 開発機の `just test` で 2 回連続再現、別ディレクトリに展開したコピーで計測バイナリを繰り返し実行して 3/20 (その後 0/185、CPU 負荷とは相関しなかった)
- 真因は tracing-core 0.1.36 の既知不具合 tokio-rs/tracing#3611 (類似 #2874、どちらも open)。登録済み Dispatch がちょうど 1 つのとき (`callsite.rs` 545-546 の `Rebuilder::JustOne`、564-565 で `dispatcher::get_default`)、callsite の interest を「その callsite を最初に通したスレッドの既定 subscriber」で決める。subscriber 無しのスレッドなら `NoSubscriber` の `Interest::never` が保存される (`callsite.rs` 499)。作り直しは次の `Dispatch::new` まで起きず、`has_just_one` は Dispatch 登録時しか更新されない (`callsite.rs` 555)
- この crate での発生順序: exchange::tests の usage 系 9 本が subscriber 無しで `request_span()` を呼ぶ。これらは名前順で最初の `logs_of` 利用試験の直前に並ぶ。最初の `logs_of` の `Dispatch::new` から次の `Dispatch::new` までの窓に usage 系が callsite を初めて通ると、never が固定される
- コードは v0.65.0 以前から同じで、タイミングだけで出る潜在競合
- 影響範囲: exchange.rs の `logs_of` 利用 13 本と、webhook.rs の `the_token_never_reaches_the_logs`。このうち `completed_stream_is_not_reported_as_aborted`、`nothing_is_recorded_when_no_chunk_arrives` の前半、`the_token_never_reaches_the_logs` は「出ないこと」を見るので、捕捉が空振りしても偽の合格になる側
- 本番経路は global subscriber が 1 つなので非該当

## 実用的な示唆 / ベストプラクティス

- 対処: `crates/llm-gateway/src/test_log.rs` に `logs_of` を 1 本化し、何も受け取らず `max_level_hint` が OFF の Dispatch (`Dormant`) をプロセス寿命で 1 つ登録して JustOne 近道を使わせない。OFF にするのは、足した瞬間に最大レベルが TRACE に上がる窓を作らないため
- 決定的な回帰試験 `a_callsite_first_seen_without_a_subscriber_still_reaches_the_capture` を同ファイルに追加した。単独実行で修正を外すと FAILED、入れると ok
- v0.66.0 に含む。tracing-core が直れば Dormant は無害な残置
- thread-local subscriber (`with_default`) でログを捕捉する試験を書くときは、同一プロセス内で subscriber 無しに同じ callsite を通る試験があると同じ罠を踏む

## 未確認

- 修正後に本物の試験が統計的に失敗しなくなったことは示せていない (修正前も後半は 0 回だったため)。根拠は決定的再現の 2 つ (回帰試験と、修正を外した状態での FAILED)

## 参考

- https://github.com/tokio-rs/tracing/issues/3611
- https://github.com/tokio-rs/tracing/issues/2874
- https://github.com/paiml/rust-mcp-sdk/pull/415 (`rebuild_interest_cache` だけでは塞がらなかった事例)
