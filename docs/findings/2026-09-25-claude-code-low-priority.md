# Claude Code の `/low-priority` が送るもの、llm-gateway との噛み合わせ

- Date: 2026-09-25

## 判明した事実

- Claude Code v2.1.282 の binary には `/low-priority` が実装済みだが、feature gate `tengu_toasty_breeze` で有効化される一部ユーザー向けで、公式 docs (commands / changelog) には未記載
- 有効時、request に `anthropic-usage-limit: slow` ヘッダを付ける (本文の `service_tier` や `anthropic-beta` ではない)
- ヘッダ送信条件: streaming は「認証形態 + mode active」の条件を満たすとき、非 streaming はこれに加えて request URL の pathname が `/v1/messages` で終わることも条件になる。pathname の末尾だけを見ており、ホスト名や base URL は見ないため、gateway 配下の別 path (例: `/ns-personal/v1/messages`) でも条件は真になる
- 応答ヘッダは `anthropic-ratelimit-unified-slow-{offer,status,retry-after,max-wait,budget-utilization}`。`status` の値は `active` / `not_needed` / `slot_busy` / `weekly_limit` / `budget_exhausted` / `ineligible` / `off`
- 429 (`slot_busy`) と 529 (`active` または欠落) を受けたクライアントは待ってから再試行する (既定で約 20 秒 ± 30%、上限は約 20 分、`retry-after` / `max-wait` があればそちらを優先)。`weekly_limit` / `budget_exhausted` / `ineligible` / `off` の場合は中断する
- UI 文言は「Continue now at lower priority」「uses your weekly limit」「You've used this week's lower-priority allowance」。通常枠の壁に当たったときに案内される。週の低優先度枠は通常枠とは別建て
- 公開 API ドキュメントの `service_tier` (`auto` / `standard_only`) とは別物。Priority Tier は Opus 5.5 は対象外
- llm-gateway 側は request ヘッダ (`anthropic-usage-limit`) も応答ヘッダ (`anthropic-ratelimit-unified-slow-*`) も中継対象から除外されておらず、素通しされる
- llm-gateway の DR-0009 は 429 / 529 を「アップストリームの断り」として即座に別経路へ切り替える設計で、429 を受けた経路には最短 60 秒の cooldown が入る。これは `/low-priority` が期待する「待てば通る」という前提と噛み合わない
  - 同一 request 内で候補経路が全滅した場合は最後の応答 (ヘッダ込み) がそのまま透過される
  - 次の request で候補経路が全て cooldown 中だと、router 自身が 429 を返すことになり `anthropic-ratelimit-unified-slow-*` ヘッダは失われる

## 実用的な示唆 / ベストプラクティス

- `/low-priority` を使う運用を想定する場合、DR-0009 の「429/529 = 断り」という前提が崩れる。429 応答の意味を判定してから経路切り替え/cooldown を適用するかどうかの分岐が必要 (別途 issue で起票済み)
- 実 request の観測には `anthropic-usage-limit` / `anthropic-ratelimit-unified-slow-*` ヘッダの記録が要るが、tap や events は現状ヘッダを記録しない。`claude --debug` の `[API REQUEST AUTH]` 行は Authorization を含むため共有・無加工保存は不可
- feature gate 対象ユーザーでないと `/low-priority` 自体を実 request で確認できない (`-p` オプションでは使えない)。課金面 (週次枠の消費) も未確認

## 検証の詳細

### binary 解析 (gpt-6-sol worker)

対象: Claude Code v2.1.282 の同梱 binary。`tengu_toasty_breeze` feature gate、`/low-priority` command 定義、ヘッダ送出条件の分岐ロジック、応答ヘッダのパース処理を確認。

考察: 実装はあるが gate 制御下にあり、公式 docs に記載がないため一般ユーザーには見えない。送出条件が pathname の末尾一致だけを見る実装のため、gateway のような path prefix を挟む中継でも条件が成立する点は gateway 側の考慮漏れリスクとして重要。

### docs 調査

対象: 公開 API docs の `service_tier` (https://platform.claude.com/docs/en/api/service-tiers)。

結果: `auto` / `standard_only` の 2 値のみで、`/low-priority` の `slow` ヘッダとは別の仕組み。Priority Tier は Opus 5.5 を対象外としている。

### gateway コード確認

対象: `crates/llm-gateway/src/egress.rs` (request ヘッダ除去リスト)、`preset/anthropic/wire.rs` (応答ヘッダ処理)、`gateway.rs` (DR-0009 の 429/529 切り替えロジック)、`preset/anthropic/metering.rs` (cooldown)。

結果: ヘッダは中継されるが、DR-0009 の断り判定と `/low-priority` の待機前提が衝突する。関連 issue: `docs/issue/2026-09-25-low-priority-slow-requests-should-wait-not-switch.md`
