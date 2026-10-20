---
title: 手元の開発全般で使う OpenTelemetry 観測基盤の検討 (Claude Code の classifier 理由から着手)
status: open
category: design
created: 2026-10-21T00:31:52+09:00
last_read:
open_entered: 2026-10-21T00:31:52+09:00
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

# 手元の開発全般で使う OpenTelemetry 観測基盤の検討 (Claude Code の classifier 理由から着手)

## 概要

手元の開発全般 (自作 daemon、各プロジェクト) で使える観測基盤の輪郭を先に決める。最初に流す source は Claude Code 1 つ。後から source が増えても送り先・面・保持を決め直さなくてよい形で置く。SaaS 分岐、長期保持、アラートは今は決めない。

## 背景

Claude Code 2.1.301〜2.1.304 で OpenTelemetry の `tool_decision` に `classifier_verdict` / `classifier_category` / `permission_mode` が入り、auto mode classifier の拒否理由を後から確認できるようになった。これまでは拒否のたびに原因を推測して allow を書き直していたので、ここの解像度を上げたい。

### 層の整理 (2026-10-21 時点)

- 仕様 / OTLP / Semantic Conventions (`gen_ai.*` を Claude Code 2.1.300 が付与) は 1 つ
- SDK: Claude Code は内蔵済み。自作ツール側は未定
- Collector (`otelcol` / `otelcol-contrib`): 単機で Backend が OTLP を直接受けるなら不要。複数 Backend への分岐、送信前のマスク、filelog 等で既存ログを拾う時に入れる
- Backend: 候補は OpenObserve (Rust 単一バイナリ、Parquet、AGPL、足跡最小) と Grafana (UI と生態系が最大、store は Loki/Tempo でなく OpenObserve / ClickHouse を datasource にも出来る)。SigNoz は ClickHouse 運用が付くので、llm-gateway の stats も OTel で 1 画面にしたくなってから

「OpenObserve か Grafana か」は store の選択と UI の選択に分けると排他ではない。

### 先に決めると後が楽になる軸

1. 面 (認証境界) の切り方。`tool_parameters` (Bash コマンド文字列、パス) に業務情報が乗るので `OTEL_LOG_USER_PROMPTS` を付けなくても境界の問題は残る。(a) 面ごとに Backend を分ける (`CLAUDE_CONFIG_DIR` と同じ思想、軽い Backend を複数) / (b) 1 つの Backend で `service.namespace` 属性で分け、見る側で絞る。kawaz の分離の強度からは (a) が自然 → OpenObserve 寄り。**kawaz 裁定待ち**
2. どこで動かすか。mbp16 常駐 (llm-gateway と同居、スリープ中は受けられないが exporter 側が再試行) か別ホストか
3. 自作ツール (llm-gateway / ccmsg / cache-warden) からの送り方。SDK を抱えるか、既存 stats / log を Collector の receiver で拾うか。ここで Collector の要否が決まる
4. 見たいものの代表例 3 つ。「classifier の拒否理由」「1 セッションの subagent 別トークン」(docs/issue/2026-09-07-stats-per-session-and-subagent.md と重なる) は検索で足りる、「llm-gateway の keepalive の効き」は時系列グラフが要る。これで Grafana のダッシュボードが要るかが決まる

## 最小の着手 (裁定後)

1. Backend を personal 面の置き場 (`~/.local/state/<backend>/`) で起動。launchd 登録は様子を見てから
2. `~/.claude-personal/settings.json` の `env` に `CLAUDE_CODE_ENABLE_TELEMETRY=1`、`OTEL_LOGS_EXPORTER=otlp`、`OTEL_METRICS_EXPORTER=otlp`、`OTEL_EXPORTER_OTLP_ENDPOINT`、`OTEL_LOG_TOOL_DETAILS=1` (`OTEL_LOG_USER_PROMPTS` は付けない)
3. classifier に止められたら `tool_decision` を検索して category を確認
4. emrd 面は personal で様子を見てから

## 関連

- docs/issue/2026-09-07-stats-per-session-and-subagent.md (session / subagent 単位の集計。OTel の resource 属性で取れる)
- DR-0024 (prompt cache keepalive。2.1.300 の `cache_creation_5m_tokens` / `cache_creation_1h_tokens` で検証できる)
- この検討は llm-gateway 固有でない。基盤として育つなら専用リポか dotfiles 側へ移す

## 受け入れ条件

- [ ] 面の切り方 (軸 1) の裁定
- [ ] Backend 選定と設置場所の決定
- [ ] Claude Code の `tool_decision` で classifier 拒否理由を検索できる
