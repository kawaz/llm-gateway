---
title: 永続化の器を Store 層として責務で切り、file backend を差し替え可能にする
status: open
category: design
created: 2026-09-15T11:27:30+09:00
last_read:
open_entered: 2026-09-15T11:27:30+09:00
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

# 永続化の器を Store 層として責務で切り、file backend を差し替え可能にする

## 概要

永続化の器を Store 層として責務(必要な一貫性の意味論)で切り出し、file backend をその 1 実装にすることで、後から別 backend (Raft 等) に差し替え可能にする。

kawaz と 2026-09-15 に議論し方向性は合意。着手指示はまだ出ていない。

## 背景

現状、unit 間の連携は全部「共有ファイル + flock」で実装されている。複数ホストでの HA (Raft 等で全体が同一メモリを持つ構成) に進むには、この器自体を差し替えられる必要がある。

今の器の一覧:

- credential (`credential/file.rs`): flock + 再読み込みの単一 writer
- keepalive の控えと発火 (`cache/keepalive/store.rs`): `.lock` を掴んだ 1 台が送る
- 日次集計 (`stats.rs`): writer 別ファイルを閲覧時に合算
- usage スナップショット (`persist.rs`): observed_at の LWW
- 締め出し / upstream status: メモリのみ

### 設計の要点

trait はファイル操作でなく **必要な一貫性の意味論** で切る:

1. **単一 writer の更新** — credential の refresh:「掴む → 最新を読む → 更新して書く」の 1 単位
2. **リース** — keepalive の系列ごとの発火担当 + 期限
3. **合算可能なカウンタ** — stats、writer 別に書いて読む時に merge
4. **LWW スナップショット** — usage、締め出し

file backend は今の実装をそのまま各 trait の実装に収める。Raft (openraft 等) backend は (1)(2) だけ log に載せ、(3)(4) は複製で済む見立て (本文 2 MB 級の控えは log に載せない)。

### 懸念

使わない抽象を先に切る YAGNI 側のコスト。ただし切る位置を意味論に置けば file 実装自体の見通しも良くなるため、HA に進まなくても損にはならないと判断。

### DR-0027 との関係

DR-0027 の「分散 backend は作らない」は、Raft backend を実際に入れる時点で supersede する。Store 層自体を切り出すことは DR-0027 と衝突しない。

### 差し替え先の候補 (2026-09-17 追記, kawaz)

第一候補は cache-warden (kawaz 製)。本来目的で使えるようになった時点で Store 層の backend にする。それまで固定 token 等の静的 secret も、今の credential と同じく生のファイルで置く。

## 受け入れ条件

- [ ] kawaz から着手指示が出たら、Store 層の trait 設計 (単一 writer 更新 / リース / 合算可能カウンタ / LWW スナップショット) を DR として起票する
- [ ] file backend を各 trait の実装として再配置する (既存挙動は変えない)

## TODO

<!-- wip 時のみ -->
