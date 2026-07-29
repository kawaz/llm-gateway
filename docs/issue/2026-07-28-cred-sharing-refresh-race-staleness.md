---
title: credential をプロセス間で共有する前提の残課題 (同時 refresh のレースと access token の陳腐化)
status: open
category: design
created: 2026-07-28T23:14:54+09:00
last_read: 2026-07-29T21:02:12+09:00
open_entered: 2026-07-28T23:14:54+09:00
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

# credential をプロセス間で共有する前提の残課題 (同時 refresh のレースと access token の陳腐化)

## 概要

llm-gateway は複数プロセス (例: 8401 stable / 8402 unstable) が同じ credential
ディレクトリ (`~/.local/state/llm-gateway/credentials/`) を共有する運用になって
いる。OAuth の refresh token はワンタイムのため、あるプロセスが refresh すると
他プロセスが持つ値は無効になる。

commit `ee91d2fc` (`fix: 別のプロセスが更新した認証情報を見落とさない`) で
`crates/llm-gateway/src/credential/store.rs` の `read()` がキャッシュヒット時に
ディスクを読まない問題を修正し、`reload()` を追加した:

- `do_refresh` の冒頭でディスクを読み直し、既に有効なら refresh せずに返る
- refresh を断られたらもう一度読み直し、有効になっていれば回復として扱う
- `record_denied_beta` も cache でなくディスクから積み直す (古い値での上書きを防ぐ)

この修正後も残っている課題が 2 つある。

### 残課題 1: 同時 refresh のレース

上記は「他プロセスが**先に完了していれば**気づく」仕組みであり、**2 つの
プロセスが同時に期限切れを検知して両方 refresh に走るレースは残っている**。

その場合、片方が成功し、もう片方は upstream に断られる。断られた側は読み直しで
回復するので致命的ではないが、無駄な API 呼び出しが起き、タイミング次第では
回復のための読み直しが「相手の書き込み完了前」に走って両方失敗しうる可能性が
ある (**この最後の点は推測であり実測していない**)。

対処の方向としてはファイルロック (flock) によるプロセス間排他が考えられるが、
tokio の async 実行下で blocking なロックをどう扱うかを含めて設計が要る。

### 残課題 2: access token 側の陳腐化 (未確認)

`acquire()` の入口の `read()` は今も cache を優先する。cache にある access
token の期限がまだ先なら、他プロセスが refresh 済みでも古い access token を
そのまま使う。

OAuth では一般に、refresh しても**古い access token は期限まで有効なまま**
なので問題にならないはずだが、**Anthropic / OpenAI の実装がそうなっているかは
確認していない**。もし refresh 時に旧 access token を無効化する実装なら、
このケースで 401 が出る。

確認方法としては、片方のプロセスで refresh を起こしてから、もう片方が持って
いる古い access token で upstream を叩いて通るかを見ることになる。

## 背景

DR-0004 で credential の json 構造 (`type` / `provider` / payload) は整理
されたが、これは「1 credential を複数プロセスがファイル経由で共有する」際の
排他制御・鮮度管理には踏み込んでいない。commit `ee91d2fc` はディスク再読込による
検知は入れたが、同時実行時の排他制御と access token の陳腐化確認は範囲外だった。

### 参考: 2026-07-28 23:00 時点の運用状況観測

- 8402 (unstable、現役): 修正後のバイナリで正常に転送している
- 8401 (stable、Caddy のフォールバック先): 修正前のバイナリで動いているため、
  cache が古いまま「refresh token が受け付けられませんでした」を出し続けている。
  credential 自体は壊れていないため、新しいバイナリで起動し直せば回復する見込み
- 8317 (旧): Caddy の転送先に含まれず誰も使っていないが稼働したまま。
  停止は権限で拒否されたため未実施

## 受け入れ条件

- [ ] 同時 refresh のレース (2 プロセス以上が同時に expired credential を
      refresh しようとするケース) の挙動を確認し、必要なら排他制御
      (ファイルロック等) を設計・実装する
- [ ] 他プロセスが refresh 済みの access token を、期限がまだ先の cache を
      持つプロセスがそのまま使い続けても問題ないか (upstream 側で旧 access
      token が無効化されないか) を確認する
