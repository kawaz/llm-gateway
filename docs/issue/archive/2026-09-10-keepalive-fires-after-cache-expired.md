---
title: keepalive がサスペンド復帰後に期限切れ cache へ盲目的に合図を撃つ
status: resolved
category: bug
created: 2026-09-10T19:28:29+09:00
last_read:
open_entered: 2026-09-10T19:28:29+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-10T21:40:00+09:00
discard_reason:
pending_reason:
close_reason: ["implemented", "dr/DR-0012", "dr/DR-0024", "done: v0.45.0 で fire()/restore()/watch_of() を Moment (単調時計+壁時計の対) 判定に変更、期限切れなら合図を出さず系列を畳む", "done: type:response event に cache フィールド (hit/written/partial/none/unknown) を usage の実結果として追加、writtenで系列起点を置き直し", "done: cache_notice (約束id) と type:cache_expired {of} (取り消し、期限切れのみ) イベントを追加", "done: MANUAL 更新、両 unit へ 2026-09-10 展開済み", "done: ccmsg 側追従は kawaz/ccmsg docs/issue/2026-09-10-gateway-cache-notice-and-expired.md に依頼済み", "note: Marker::Late 判定 (Pending.deadline: Instant) は単調時計のまま残留、別issue候補"]
blocked_by:
origin: 自リポ TODO
---

# keepalive がサスペンド復帰後に期限切れ cache へ盲目的に合図を撃つ

## 概要

サスペンド復帰後に keepalive が期限切れの cache へ盲目的に合図を撃ち、1h write
(2 倍単価) の再構築を起こす。

## 背景

実測 2026-09-10: MBP がバッテリ切れでサスペンド (uptime 16 日 = 再起動ではない)、
復帰直後 10:02〜10:19 UTC に統括セッションへ 4 本の ping が配送された。

原因: `crates/llm-gateway/src/cache/keepalive.rs:824` `fire()` は経路の可用性は
見るが cache の `expires_at` が過ぎているかを撃つ前に確認しない (期限超過は戻りで
`Late` に分類されるだけ)。期限切れ系列を捨てるのは `restore()` (プロセス再起動時)
のみ。

加えて Rust の `Instant` は macOS でスリープ中に進まないため、復帰後も tokio
タイマーと `expires_at: Instant` は「期限内」と判断し、壁時計では cache が消えて
いるのに予定どおり撃つ。

方針: `fire()` で壁時計 (`now_unix_ms`) と系列の `expires_at` の Unix ms (置き場と
同じ値) を比較し、期限切れなら合図を出さずに系列を畳む (次の実リクエストで張り直す)。
監督者や `restore` の `Instant` 判定も壁時計を正に。

関連: DR-0024 §2、DR-0027 (自送信方式でも同じ判定が要る)。

## 受け入れ条件

- [ ] `fire()` が壁時計ベースで期限切れ系列を検出し、合図を出さずに畳む
- [ ] 監督者 / `restore()` の期限判定も壁時計 (Unix ms) に統一される
- [ ] テスト: 期限を過ぎた系列は fire しないことを確認
- [ ] テスト: 壁時計が飛んだ (Instant は進まない) スリープ/復帰状況を模擬して確認
