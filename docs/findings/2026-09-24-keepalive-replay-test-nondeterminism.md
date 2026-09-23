# Keepalive の復元時に古い timer が残る

- Date: 2026-09-24

## 判明した事実

- Release workflow 35880947364 の Ubuntu job で `what_was_kept_is_picked_up_again` が `left: 2, right: 1` で失敗した。同じ revision の他 job では通過した。
- 実証済みの実装 bug (対処済み): `Keepalive::plan` が `Arc<Keepalive>` を timer task に移し、`Keepalive` は task の `JoinHandle` を `timers` に保持していた。外部からの最後の `Arc` を落としても循環参照が残り、古い timer が終了しない (= drop した Keepalive の timer が生き続けるリーク)。対処は timer task に `Weak<Keepalive>` を渡し、発火時に upgrade できる場合に限って `fire` すること。所有者が落ちると timer 自身も drop され、task が abort される。テストには所有者の消滅を直接確かめる assertion を追加した。
- 当該テストは `#[tokio::test(start_paused = true)]` と `tokio::time::advance` を使い、timer は仮想時刻。ただし期限切れ判定には壁時計の Unix ミリ秒を使用する。

## 未立証: CI の `2 != 1` との因果

上の循環参照だけで CI の二重送出を説明できるかは未立証。`fire` は store の `claim` (系列 lock) と `kept.fires_at_ms > expected_ms` の検出を備えるので、古い timer と新しい timer が共存しても同じ予定を 2 回送ることは原理上抑止されるはずで、すり抜ける interleaving はまだ特定できていない。別の候補 (`restore` 直後の初回発火と timer 発火の重複、仮想時刻と壁時計の Unix ms の境界、`advance` 量と期限の関係) を含めて再現条件を調査中。

## 実用的な示唆 / ベストプラクティス

復元試験では送出本数だけでなく、復元前の所有者が確実に解放されたことも検証する。task handle を所有する object の task が同じ object を強参照すると、外部所有者の drop は停止条件にならない。

## 検証の詳細

| 検証 | 結果 |
|---|---|
| GitHub Actions の失敗ログ | Ubuntu の 889 テスト中、このテストだけ `2 != 1` で失敗 |
| 修正前の focused test | 30 回成功。二重送出は実行順序依存で、単独の反復では再現しなかった |
| 所有者消滅 assertion を修正前コードに追加 | `the old owner stopped watching` で確実に失敗し、循環参照を実証 |
| Weak 参照へ修正後の focused test | 50 回成功し、所有者消滅 assertion も通過 |

未確認: Ubuntu の当該 workflow と同じ負荷条件下での修正後の再実行。本機は macOS であり CI runner のスケジューリングは再現できない。
