---
title: keepalive の復元テストが CI で 1 回だけ 2 回送出 (`2 != 1`) した真因が未特定
status: open
category: bug
created: 2026-09-24T00:40:50+09:00
last_read:
open_entered: 2026-09-24T00:40:50+09:00
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

# keepalive の復元テストが CI で 1 回だけ 2 回送出 (`2 != 1`) した真因が未特定

## 概要

Release workflow run 35880947364 (commit 575fade、ubuntu-latest) で `cache::keepalive::tests::what_was_kept_is_picked_up_again` が `assertion left == right failed: it replayed on the old schedule / left: 2, right: 1` で fail。同 commit の他 3 run (CI ×2、Release のもう 1 本) と macOS では pass。**flaky として扱わない** (test-integrity)。真因が特定できるまで open。

## 背景

docs/findings/2026-09-24-keepalive-replay-test-nondeterminism.md 参照。

- 調査で見つかった別 bug (実証・v0.53.1 で修正済み): `Keepalive::plan` が timer task に `Arc<Keepalive>` を強参照で渡し、drop 後も timer が残る循環参照。`Weak` に変更し、所有者消滅の assert を追加
- ただし `fire` は store の `claim` (flock、同一プロセス内も競合) と `kept.fires_at_ms > expected_ms` の検出を備えるので、旧新 timer の共存だけでは同じ予定を 2 回送れない。**CI の 2 != 1 との因果は未立証**
- 修正後の focused test は 300 回 pass。修正前も単独 30 回 pass (実行順序依存で再現せず)。本機は macOS で CI runner のスケジューリングは再現不能

## 未検証の仮説

- テストは `#[tokio::test(start_paused = true)]` + `tokio::time::advance`。テスト内の `timeout(60s)` 待ちで paused clock が自動進行し、task の実行順によっては次の 55 分の timer まで進んで 2 回目が発火する
- 期限切れ判定は壁時計の Unix ms を使うため、仮想時刻との境界で判定が変わる
- `restore` 直後の初回発火と timer 発火の重複

## 受け入れ条件

- [ ] 2 回送出が起きる interleaving を特定する (仮説のどれか、または別)
- [ ] 特定した条件を決定的に再現するテストを書く (負荷や sleep に依存しない形)
- [ ] 実装の bug なら修正、テスト設計の問題なら決定的な形に直す (assert の緩和・timeout 延長は不可)

### 否定した仮説 (2026-09-24、worker の論理検証)

- 「`timeout(60s)` 待ちで paused clock が自動進行し次の 55 分 timer に届く」: 仮想 advance は 55 分 + 1 秒、`until_sent` の timeout は最大 60 秒で、次の 55 分 timer には届かない
- 「`restore` 直後の初回発火と timer 発火の重複」: `restore()` は `store.load_all` → `plan` のみで直接 `fire` しない (keepalive.rs:374-403)
- 「旧新 timer の共存で同じ予定を 2 回」: `claim()` の排他 (store.rs:149-175)、負け側は +55 分へ再計画 (keepalive.rs:453-461)、勝ち側が save した後の旧 expected は `fires_at_ms > expected` で送出回避 (:490-499)。直接の二重送出経路は見つからない

残る候補: 期限切れ判定の壁時計 Unix ms と仮想時刻の境界、`sender.send` 内で count が増えるタイミングと save 完了の差 (`until_sent(1)` が戻る観測時刻のズレ)、他テストとの共有状態 (env / tempdir / flock)。full library 30 回 × 889 件と focused 300 回で本機 (macOS) では再現なし。
