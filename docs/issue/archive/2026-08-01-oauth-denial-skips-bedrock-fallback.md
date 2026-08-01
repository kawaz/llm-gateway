---
title: Claude サブスク認証なしのクライアントで、bedrock の fable が開いていても OAuth の枠判定で止まる
status: resolved
category: bug
created: 2026-08-01T22:30:21+09:00
last_read:
open_entered: 2026-08-01T22:30:21+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-08-01T23:45:55+09:00
discard_reason:
pending_reason:
close_reason: ["finding/2026-08-01-client-side-quota-precheck", "done:gateway側のバグではなくClaude Code(fable)がapi.anthropic.comへweekly_scoped枠を直接事前確認しリクエスト送信前に打ち切っていたと判明、gatewayには1本も届かずBASE_URL経由の判断材料が無いためgateway側からの介入は不可、回避策はANTHROPIC_AUTH_TOKENへダミー値設定(ただし/statusのサブスク表示は失われる)"]
blocked_by:
origin: main
---

# Claude サブスク認証なしのクライアントで、bedrock の fable が開いていても OAuth の枠判定で止まる

## 概要

ANTHROPIC_AUTH_TOKEN なしで gateway に繋いでいるセッションで、fable のリクエストが「OAuth credential の fable 枠 (weekly_scoped) が上限」という判定で止まってしまう。実際には bedrock 経路の fable が開いており、試せば通るのに試しすらせずにリミット判定で打ち切られる。

## 背景

### 関係する実装 (DR-0009 / DR-0007)

- v0.10.0 で `GET /api/oauth/usage` の `limits[]` から scoped denial を作る仕組みを入れた (weekly_scoped の Fable 枠 → `claude-fable-*` パターンを当該 credential で締め出す)
- この締め出しは OAuth credential に対するものだが、routing の候補には bedrock も並んでいる。OAuth 側が全部 denied になった時に bedrock を試す前に打ち切られているように見える

### 観測 (統括、2026-08-01 13:30 頃、unstable 11301)

- ログ: 「枠を聞いて締め出しを引き直しました credential=claude-emrd limits=3 denied=true」「credential=claude-kawazzz ... denied=true」(zunsystem は denied=false)
- 同時刻の実疎通では `claude-fable-5` が 200 で通る (bedrock 経由と思われる)
- つまり「denied な OAuth が並んでいる状態」と「bedrock で通る状態」が併存しており、条件によって前者で打ち切られる

### 期待

OAuth の枠で締め出されていても、routing に残っている bedrock 等の別 credential を試すべき。全滅時のみ denial を返す (DR-0009 の全滅時挙動)。

### 調査の起点

- `crates/llm-gateway/src/denial.rs` の `candidates()` — 締め出しで候補が空になった時の扱い
- `crates/llm-gateway/src/gateway.rs` の `Candidates::AllDenied` 分岐 — 「どの経路も断られている」判定に bedrock が含まれているか (bedrock は OAuth 枠と無関係なのに巻き込まれていないか)
- scoped denial (`Scope::Models`) が credential 単位に正しく閉じているか、モデルパターンだけで別 credential まで巻き込んでいないか

### 補足

- 回避策は複数ありそうだが、まずは起票のみ。実装方針は未定
- 再現条件の切り分けが必要: 「AUTH_TOKEN なしのクライアント」という条件が本当に効いているのか、単に OAuth 全滅のタイミングだったのかは未確認

## 受け入れ条件

- [ ] OAuth credential が全て denied でも、bedrock 等の別経路の候補が残っていればそちらを試す
- [ ] 全経路が denied/失敗の時のみ denial を返す (DR-0009 の全滅時挙動)
- [ ] 再現条件 (AUTH_TOKEN なしクライアント固有か否か) を切り分けて記録する
