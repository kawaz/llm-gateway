# credential store の並行挙動 (プロセス内 / プロセス間)

調査日: 2026-07-29。対象 commit: v0.2.1 時点。行番号は当時のもの。

## 判明した事実

- プロセス内の refresh は `in_flight` (broadcast による single-flight) で 1 回に直列化される
  (store.rs:181-216、テスト `concurrent_acquires_trigger_exactly_one_refresh` で確認)。
  ロックはいずれも await を跨がない。
- single-flight が守るのは「refresh の実行」のみ。**read-modify-write 区間は守らない**:
  `record_denied_beta` (reload→clone→store) と `do_refresh` (reload→HTTP→store) は互いに
  排他しておらず、同一プロセス内でも lost update が起きる (denied_beta の記録消失)。
- プロセス間のレース窓は 3 つ:
  - **窓 A**: do_refresh の reload (L224) 〜 POST (L243)。両プロセスが同じ refresh token で
    POST し、片方が invalid_grant で失敗
  - **窓 B**: 拒否側の回復 reload (L262) が成功側の store (L290) より先に走ると回復失敗。
    リトライ・待機なしの 1 回きり。構造的に成立する (発生確率は未実測)
  - **窓 C**: do_refresh の書き込みは L224 時点のスナップショット由来なので、POST 中に
    他プロセスが書いた denied_beta 等を丸ごと消す (lost update)
- commit `ee91d2fc` の再読込がカバーするのは「相手が**完了済み**」の場面のみ。同時突入
  (窓 A)・書き込み前の読み (窓 B)・lost update (窓 C) はカバーしない。
- 同時突入の現実的トリガ: `keep_models_fresh` が既定 3600 秒間隔で全 credential に
  acquire する。期限切れは複数プロセスで同時刻に成立する。
- **ディスク書き込みの tmp ファイル名が固定** (`<id>.json.tmp`、file.rs:60)。
  tmp→sync→rename 自体は原子的だが、2 プロセスが同時に store すると同じ tmp を
  truncate で開き合い、壊れた JSON が rename されうる (コード構造上成立、未実測)。
- **cache に evict は無い** (insert のみ、生存期間 = プロセス寿命)。acquire は cache の
  access token の期限が切れるまでディスクを見ないため、他プロセスの refresh 結果・
  denied_beta 学習・手動編集 (priority 等) は期限まで反映されない。
- flock 系 crate への直接依存は無い。spawn_blocking の使用実績も無い。credential の
  ファイル I/O は std::fs の同期呼び出しを async 上で直接実行している。
- refresh 失敗時はファイル無変更。成功時は access_token / expired / last_refresh を更新、
  refresh_token は応答に含まれる場合のみ差し替え。
- テスト偽装: `Spy` (Persistence 差し替え、swapping で他プロセス書き込みを模擬) と
  `FakeTokenServer` (実 TCP、遅延・拒否を注入)、`Clock::Fixed`。Spy は同一プロセスの
  StdMutex なので真のプロセス間排他 (flock) は検証できない。flock は同一プロセスの
  別 fd 同士でも競合するため、FileStore 2 インスタンスでの検証は可能。

## 実用的な示唆

- プロセス間排他は credential 本体ファイルへの flock では**壊れる** (rename で inode が
  変わり、rename 前後で別 inode をロックして排他が破れる)。`.lock` サイドカーが必要。
- ロック挿入点は「do_refresh の reload〜store」+「record_denied_beta の reload〜store」を
  同一ロックで囲むのが自然。プロセス内 single-flight の外側なのでリーダー 1 タスクしか
  ロックに来ず、ロック取得後の reload が double-check として機能して窓 A/B が同時に閉じる。
- ロック保持時間は HTTP refresh 往復ぶん。flock の待機は同期呼び出しなので
  spawn_blocking へ逃がす (flock は解放時に起床するので polling ではない)。
- 詳細な設計判断は DR-0010 参照。

## 検証の詳細

read-only のコード読解による (opus5-high worker、2026-07-29)。実測を伴わない箇所は
本文中に (未実測) と明記した。フロー全体:

```
acquire (111)
├ read (144) ──cache hit(145)→ 返す              ← disk を読まない
│              └ miss → reload (156) = load(157) + cache.insert(158-161)
├ needs_refresh(114/165) false → to_credential(295) で返す
└ refresh_once (181)
  ├ in_flight に先着あり → broadcast を待つ (187-200)
  └ 先着なし → do_refresh (223)
     ├ reload (224)                              ← ee91d2fc の完了検知
     ├ needs_refresh false → 早期 return (239)
     ├ oauth::refresh_at (243)                    ← upstream POST
     │  └ Err → reload (262) 1 回だけ → 回復 or 元エラー
     └ Ok → next 組み立て (272-285) → store(290) → cache.insert(291)

record_denied_beta (128) = reload (134) → clone+record → store(138) → cache.insert(139)
書き込みは store.rs:138 / 290 の 2 か所。FileStore::store は tmp→sync_all→rename。
```
