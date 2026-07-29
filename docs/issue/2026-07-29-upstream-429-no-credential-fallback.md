---
title: gateway が upstream 429 で次 credential にフォールバックしない
status: open
category: bug
created: 2026-07-29T11:21:46+09:00
last_read:
open_entered: 2026-07-29T11:21:46+09:00
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

# gateway が upstream 429 で次 credential にフォールバックしない

## 概要

gateway が upstream から 429 (レート制限) を受け取った際、routing 定義上
次候補の credential が存在していてもそちらへフォールバックせず、429 を
そのままクライアントへ透過している。複数 credential を登録する運用目的の
一つはレート制限回避のはずで、現状はその効果が得られていない。

## 背景

複数 credential (アカウント) を登録する運用は、単一 credential が
レート制限に達しても他のアカウントで継続処理できることを期待している。
429 でフォールバックしないと、この運用上のメリットが得られず、他の
credential が空いていても全体が詰まってしまう。

`crates/llm-gateway/src/gateway.rs` の既存テスト (`FakeUpstream::always(429)`,
gateway.rs:728 / gateway.rs:754) は「429 がそのまま返る」ことを確認している
だけで、複数 credential 間のフォールバックは検証していない。

## 実測 (2026-07-29)

1. **現象**: `/ns-personal/v1/messages` に `claude-opus-5` を投げると
   `route="claude-kawazzz"` が upstream 429 (`rate_limit_error`) を返し、
   そのままクライアントへ透過される。routing 定義上は次候補
   (`claude-zunsystem`) があるのに試行されない。
2. **再現コマンド**:
   ```
   curl http://127.0.0.1:8402/ns-personal/v1/messages \
     -H 'Authorization: Bearer personal' \
     -H 'anthropic-version: 2023-06-01' \
     -d '{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
   ```
   → 429 (当時 upstream が opus/sonnet に 429 を返す状態だった)。
   ログ: `~/.local/state/llm-gateway/logs/unstable.log` の
   2026-07-29T02:15 前後、`model=claude-opus-5 route="claude-kawazzz"
   status=429` が連続して記録されている。
3. **関連の未解明点** (裏取りしてから採否を決めること): ns-emrd の
   `claude-opus-5` リクエスト (req=3604) が routing 先頭の `claude-emrd`
   でなく `claude-kawazzz` に route されていた。discovery のモデル一覧、
   または credential スキップ条件が原因の可能性がある。本 issue の
   フォールバック調査と合わせて確認する。

## 論点

429 を「credential 固有の枯渇」とみなして次候補にフォールバックするか、
「モデル/アカウント横断の上流事情」とみなしてそのまま透過するかは設計判断。
`Caddyfile` (canddy-app-proxy) は 429 で upstream を切り離さない設計を
意図的に選んでいるとみられ、その理屈との整合も検討対象。

## 受け入れ条件

- [ ] 429 応答時に次候補 credential へフォールバックすべきか、設計判断
      (Caddyfile の既存方針との整合含む) を確定する
- [ ] フォールバックする場合、全 credential が 429 の場合の挙動を明確化する
- [ ] ns-emrd のルーティング先が routing 先頭と異なっていた件の原因を
      裏取りする (discovery のモデル一覧 or credential スキップ条件を疑う)
- [ ] 複数 credential を用いた 429 フォールバックのテストケースを追加する
