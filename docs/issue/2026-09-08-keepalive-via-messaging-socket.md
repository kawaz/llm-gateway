---
title: keepalive の合図を messaging socket へ直接注入する
status: open
category: design
created: 2026-09-08T16:50:47+09:00
last_read:
open_entered: 2026-09-08T16:50:47+09:00
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

# keepalive の合図を messaging socket へ直接注入する

## 概要

keepalive の合図を ccmsg 経由でなく Claude Code の messaging socket へ直接注入する
(DR-0024 §2 の届け先差し替え)。

## 背景

根拠: `docs/research/2026-09-08-session-injection-and-json-port.md` —
`${XDG_RUNTIME_DIR:-$TMPDIR}/cc-socks/<pid>.sock` に
`{"type":"user","message":{"content":"<marker>"}}` を 1 行書くだけで idle セッションが
即起動し、macOS では認証不検証。

kawaz 2026-09-08: 「webui のゴミ対応 (nonce 1 行返しルール) もこれで無くせそう」。

効果: webhook → ccmsg → Monitor 通知の経路が不要になり、webui に ping が流れない。

関連: claude-ccmsg リポ `docs/issue/2026-09-08-messaging-socket-direct-write.md`
(ccmsg 側の同論点)、keepalive-daily-counters。

(category: design — 要確認。enhancement 相当だが本リポの enum に無いため最も近いものを選択)

## 受け入れ条件

- [ ] 検証すべき点 (1)〜(4) を実機で確認し、方針を確定する
- [ ] 方針確定後、DR-0024 §2 の届け先差し替えとして実装する

## 検証すべき点

- (1) 受信コードは socket 由来の user メッセージを `isMeta: true` でキューに入れる —
  TUI transcript に ping と応答が表示されないなら応答文言の縛り
  (llm-gateway-cache-keepalive ルール) も不要になる。実機で確認。
- (2) session_id → pid → socket の対応取り: gateway は X-Claude-Code-Session-Id しか
  知らない。`claude agents --json` は CLAUDE_CONFIG_DIR でスコープされるので、
  全 config dir を舐めるか、socket 側の `session_id` 一致必須の性質を使って総当たりで
  書く (不一致は黙殺される) か。
- (3) `from` を省略 (`unknown`) した時の受信側の挙動 (返信先無しで `failed` に
  ならないか)。
- (4) marker の合言葉消費判定 (DR-0024 §2-4) がエンベロープ込み本文で成立するか。
