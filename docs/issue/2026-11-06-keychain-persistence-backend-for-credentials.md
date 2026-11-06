---
title: credential の置き場に macOS keychain backend を足す
status: open
category: design
created: 2026-11-06T16:06:05+09:00
last_read:
open_entered: 2026-11-06T16:06:05+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: 自リポ TODO (kawaz 発案 2026-11-06)
---

# credential の置き場に macOS keychain backend を足す

## 概要

credential の置き場 (`~/.local/state/llm-gateway/credentials/*.json`、0600) は同じユーザのプロセスなら誰でも読める。AI エージェントが同じユーザで多数走る環境ではここが実質の弱点なので、DR-0031 の (1) `Persistence` trait (`crates/gateway-core/src/credential.rs`、実装は `FileStore` のみ) の 2 つ目の backend として macOS keychain を足し、`store = "keychain"` (DR-0031 決定 4) で選べるようにする。cache-warden を待つ「つなぎ」ではなく恒久の backend として位置づける。

item には code signature の要件 (Team ID の designated requirement、ダイアログ無し) の ACL を付け、llm-gateway の binary 以外は読めなくする。

用語: これは Security.framework の keychain ACL で TCC ではない。

## 背景

kawaz 発案 (2026-11-06)。着手は DR-0042 / DR-0043 のリリース後。

## DR で決めること

1. 排他: `.lock` の flock を sidecar として残すか、item の attribute の版で楽観ロックにするか
2. 版: mtime の代わりに attribute の連番 / 更新時刻
3. 署名 identity: dev build と unstable を Developer ID でローカル署名する手順 (justfile)、未署名 binary の扱い
4. login keychain が locked の時 (再起動直後の未ログイン、ssh のみ) は fail-closed にし、file backend へ落ちない
5. Linux では variant を出さない

## 先に行う実機マトリクス (1 段)

`security-framework` crate か FFI で ACL 付き item を作り、次を確かめる。`SecAccess` / `SecTrustedApplication` は legacy API で crate の対応が薄い。

- 同 Team ID の別 binary から読める
- 別署名の binary から読めない
- 未署名の binary から読めない
- いずれの場合もダイアログが出ない

## 受け入れ条件

- [ ] 実機マトリクスの結果を `docs/findings/` に記録
- [ ] 上記 5 点を決めた DR を起こす
- [ ] `store = "keychain"` で選べる keychain backend を実装 (macOS のみ)
- [ ] dev build / unstable のローカル署名手順を justfile に追加
