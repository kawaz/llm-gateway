---
title: エコシステム外部レビュー(2026-09)の指摘への対応検討
status: resolved
category: task
created: 2026-09-10T14:57:18+09:00
last_read:
open_entered: 2026-09-10T14:57:18+09:00
wip_entered:
blocked_entered:
pending_entered:
discarded_entered:
resolved_entered: 2026-09-10T15:20:00+09:00
discard_reason:
pending_reason:
close_reason: ["triage-recorded","L-3-needs-kawaz-decision"]
blocked_by:
origin: kawaz依頼(2026-09-10、claude-rules-personalセッション経由)
---

# エコシステム外部レビュー(2026-09)の指摘への対応検討

## 概要

外部レビュー (2026-09-09〜10、`claude-rules-personal` リポ
`docs/research/2026-09-10-ecosystem-review/llm-gateway.md` + `common.md`)
のうち本リポ向けの指摘 (L-1, L-3, L-4, L-5) を実物と照合し、採否を記録する。

レビューは初版の指摘から個別プロジェクトの精読を進めるたびに認識が改まり、
指摘が覆されたケースが多いとの前提付きだったため、鵜呑みにせず repo 実体
(DR の Status フィールド、issue frontmatter、実ファイルの行数) と照合した。

## 照合結果と裁定

### L-1 (★1, README-ja ペア) — 採用 (低優先度、着手は未定)

- 照合: `README.md` のみ存在、`README-ja.md` は無い。指摘は正確
- 裁定: `translation-pairs.md` 規約どおり README-ja.md を原本化する対応は妥当。
  ただし急がない案件のため、着手タイミングは担当セッションに委ねる

### L-3 (★3, DR-0027 の Accept 判断) — 裁定待ち (kawaz 要判断)

- 照合: `docs/decisions/DR-0027-keepalive-by-replay.md` は `Status: Proposed`
  のまま。不確定要素の解消は指摘の「9/9」ではなく **9/8** の
  `docs/findings/2026-09-08-cache-ttl-refresh-on-hit.md` (同一 body replay +
  max_tokens=1 で TTL 延命することを実測済み) が根拠。日付の指摘は誤りだが、
  「不確定は解消済みで Accept 可能な状態」という主張自体は事実
- 矛盾の指摘 (`docs/issue/2026-09-08-keepalive-via-messaging-socket.md` が
  open のまま) も事実確認できたが、**DR-0027 本文の「却下した案」節で既に
  この案を明示的に却下済み**。矛盾は設計上のものではなく、issue 側の
  status 更新が追いついていないだけの housekeeping 漏れ
- 裁定待ち: DR-0027 を Accepted にして実装に入るか (§3→§1→§6→§7 の順)、
  見送って理由・再開条件を DR に書くかは **設計判断そのもの**なので
  kawaz の判断が要る。本 issue では採否を確定しない
- housekeeping (`keepalive-via-messaging-socket` issue の close) は
  DR-0027 の裁定と連動して処理するのが自然なため、こちらも保留する

### L-4 (★2, DR-0028 の「未確定」節を閉じる) — 採用

- 照合: `docs/decisions/DR-0028-daemon-service-subcommands.md` は
  Accepted・v0.44.0 で実装済みだが、`## 未確定` 節の 3 項目
  (子ログの置き場と回転 / 監督者が落ちた時の子の扱い / systemd 未検証) は
  未回答のまま残存。派生 issue
  `docs/issue/2026-09-09-daemon-restart-order-and-grace.md` と
  `docs/issue/2026-09-09-service-status-running-false-while-loaded.md` も
  実在し共に open。指摘は正確
- 裁定: 採用。実装を読んで決定10〜12として DR-0028 に追記する対応で妥当。
  設計判断を伴わない (実装済み挙動の記録) ため kawaz 裁定は不要。
  着手タイミングは担当セッションに委ねる

### L-5 (★1, gateway.rs / lib.rs の分割) — 採用 (急がない)

- 照合: `wc -l` で `crates/llm-gateway/src/gateway.rs` = 7070 行、
  `crates/llm-gateway-server/src/lib.rs` = 4089 行。指摘は正確。
  DR-0014 (Accepted) の ingress/egress/exchange 境界はまだファイル構造に
  反映されていない
- 裁定: 採用するが、指摘自身が「テストが厚いので急がない」と明記しており
  分割単独の PR は作らない方針も妥当。次に gateway.rs を大きく触る機会に
  DR-0014 §1 の語彙で割る

## 受け入れ条件

- [x] L-1, L-3, L-4, L-5 それぞれを repo 実体と照合した
- [x] 採否 (採用/裁定待ち) と理由を記録した
- [x] L-3 は設計判断そのものであり kawaz の裁定待ちとして明示した (自動で
      Accept/却下を確定しない)
