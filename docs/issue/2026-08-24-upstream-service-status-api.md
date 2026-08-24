---
title: upstream LLM service の状態を統一 API で一括表示する
status: open
category: request
created: 2026-08-24T16:10:46+09:00
last_read:
open_entered: 2026-08-24T16:10:46+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered:
discard_reason:
pending_reason:
close_reason:
blocked_by:
origin: ユーザ要望
---

# upstream LLM service の状態を統一 API で一括表示する

## 概要

Anthropic、OpenAI/Codex 等の公式 service status と、llm-gateway が実通信で
観測した状態を一括取得できる `GET /llm-gateway/status` を追加する。

provider ごとの公式 JSON の違いは gateway 内の adapter で吸収し、利用側には
provider 共通の JSON schema を返す。`llm-gateway status` CLI と、後続の ccmsg
クオータ画面・global header 警告はこの HTTP API を正本として使う。

詳細設計は [DR-0021](../decisions/DR-0021-upstream-service-status.md) を正本とする。

## 背景

2026-08-24 の Anthropic 障害では、Claude OAuth 3 route がすべて origin の
`api.anthropic.com` から 529 を受けた。gateway は正しく route fallback して最後の
529 を透過したが、利用者は gateway log、quota、Anthropic status page を別々に
確認しないと「gateway 障害ではなく origin 全体の障害」と判断できなかった。

現在の `/llm-gateway/healthz` は Caddy が使う daemon liveness で、upstream へ
触れないことが不変条件である。credential quota の `/llm-gateway/usage` とも
意味と更新契機が異なるため、独立した status endpoint が必要。

## 公式 endpoint の確認結果

### Anthropic

- `https://status.claude.com/api/v2/summary.json`
- `https://status.claude.com/api/v2/incidents/unresolved.json`
- 対象 component: `Claude API (api.anthropic.com)`

### OpenAI / Codex

- `https://status.openai.com/api/v2/summary.json`
- `https://status.openai.com/api/v2/incidents.json`
- 対象 component: `Codex API`
- `incidents/unresolved.json` は 404。全 incident から `resolved` / `postmortem` を
  gateway 側で除外する

両者は共通 `statuspage_v2` adapter で扱い、`summary_url` / `incidents_url` の
違いだけを source config に持たせる。外向き JSON にこの差を漏らさない。

AWS Health API は AWS 認証と対象 support plan が必要なため、v1 は official page の
link と gateway 実測だけを返す。HTML scraping は行わない。

## 受け入れ条件

- [ ] `GET /llm-gateway/status` が configured route を service 単位にまとめ、
  provider 共通 schema の JSON を `200 OK` で返す
- [ ] response が `schema_version`、top-level `overall.severity`、各 service の
  `severity` / `routes` / `official` / `observed` を持つ
- [ ] `official.state` が `operational | degraded | partial_outage | major_outage |
  maintenance | unknown` に正規化される
- [ ] Anthropic と OpenAI/Codex の公式 endpoint を同じ `statuspage_v2` adapter で
  取得でき、component filter が対象外 service の障害を誤適用しない
- [ ] source の timeout / malformed JSON / oversized body / unknown state が
  他 service を巻き込まず、最後の成功 snapshot または `unknown` として返る
- [ ] gateway 実測は `reachable | failing | unknown` を公式状態と別 field で返し、
  401 / 403 / 429 を service failure と誤認しない
- [ ] 529、transport error、`ResponseAdmission::Busy` が対象 source の background
  refresh を起動し、元の LLM request は status refresh を待たない
- [ ] 定期 refresh、`?refresh=true`、failure-triggered refresh が source ごとの
  single-flight と cooldown で過剰 request にならない
- [ ] source 未指定 route も report から消えず、official `unknown` と実測を返す
- [ ] `/llm-gateway/healthz` の意味・応答・Caddy health check への影響が変わらない
- [ ] `llm-gateway status [--refresh]` が同 endpoint を人間向けに表示する
- [ ] DR-0021 の test scope を満たす unit / integration test が追加される
- [ ] ccmsg 側へ渡せる schema と polling / 529 refresh trigger の連携仕様が
  DR-0021 と矛盾なく確定している

## 実装単位

1. config と正規 schema (`StatusConfig`, `StatusSourceSpec`,
   `RouteSpec.status_source`)
2. `statuspage_v2` adapter、bounded fetch、component / incident 正規化
3. cache、stale、single-flight、periodic / explicit refresh
4. route attempt observer と failure-triggered background refresh
5. HTTP endpoint と CLI formatter
6. ccmsg 組み込み依頼の handoff

## 非対象

- Caddy が 529 を受けたとき stable daemon へ再送すること
- status page の結果を routing や credential denial に使うこと
- AWS Health 用 IAM credential の新設
- OpenAI / AWS status page HTML の scraping
- ccmsg の実装そのもの。この issue では llm-gateway API と handoff 仕様までを扱う

