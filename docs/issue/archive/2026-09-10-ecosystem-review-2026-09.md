---
title: エコシステム外部レビュー (2026-09) の指摘への対応検討
status: resolved
category: task
created: 2026-09-10T14:57:18+09:00
last_read:
open_entered: 2026-09-10T14:57:18+09:00
wip_entered:
blocked_entered: 2026-09-10T15:30:00+09:00
pending_entered:
discarded_entered:
resolved_entered: 2026-09-10T23:28:32+09:00
discard_reason:
pending_reason:
close_reason: ["finding/2026-09-10-ecosystem-review-triage","dr/DR-0027:implemented","done:採用4件実施済み(README/DR-0028/--/L-5は次の大改修時)","done:派生issue起票済み(supervisor-crash-leaves-orphan-children, supervisor-log-reopen-on-rotation)","done:keepalive-via-messaging-socket discard","done:keepalive-daily-counters 書き換え済み"]
blocked_by: kawaz 裁定 (L-3: DR-0027 を Accepted にするか) — 2026-09-10 裁定完了 (QUESTIONS KA-Q1=a) で解消
origin: kawaz 依頼 (2026-09-10、claude-rules-personal セッション経由)
---

# エコシステム外部レビュー (2026-09) の指摘への対応検討

## 概要

外部レビューで本リポ (llm-gateway) 向けの指摘が出た。以下 2 ファイルを読んで対応を検討する。

- 個別ファイル: `/Users/kawaz/.local/share/repos/github.com/kawaz/claude-rules-personal/main/docs/research/2026-09-10-ecosystem-review/llm-gateway.md`
- 共通ファイル: `/Users/kawaz/.local/share/repos/github.com/kawaz/claude-rules-personal/main/docs/research/2026-09-10-ecosystem-review/common.md`

## 背景

kawaz からの依頼 (2026-09-10、claude-rules-personal セッション経由)。レビューは初版の指摘から個別プロジェクトの精読を進めるたびに認識が改まり、指摘が覆されたケースが多い。**全面的に鵜呑みにせず実物と照合してから採否を決めること**。「裁定待ち」項目は kawaz の判断が要る。対応タイミングは担当セッションまたは kawaz に任せる。

## 受け入れ条件

- [x] 個別ファイル・共通ファイルの指摘を実物 (本リポのコード・DR・issue) と照合する
- [x] 各指摘について採用 / 却下と理由を判定する (裁定が要るものは「裁定待ち」として明示)
- [x] 採否の結果を本 issue に追記して close する

## 採否の結果 (2026-09-10)

照合は `docs/findings/2026-09-10-ecosystem-review-triage.md`。

### 採用 (4)

- **L-1**: README の ja/en ペア化 → 実施済み (v0.44.3)
- **L-4**: DR-0028 の未確定 3 項目を決定 10〜12 に昇格 → 実施済み
- **L-5**: gateway.rs / server lib.rs の分割 → 採用。着手は次に大きく触るタイミング、分割だけの PR は作らない
- **C-1**: 外部コマンドの `--` → 実施済み (tail と open)

### 既に対応済み (1)

- 詳細は triage findings 参照

### 却下 (0)

該当なし

### 派生起票

- `supervisor-crash-leaves-orphan-children`
- `supervisor-log-reopen-on-rotation`

### 裁定完了 (1)

- **L-3**: DR-0027 (keepalive を自送信に置き換える) は kawaz 裁定 (2026-09-10、
  QUESTIONS KA-Q1 = a) で Accepted、実装着手済み。`keepalive-via-messaging-socket`
  は discard、`keepalive-daily-counters` は書き換え済み

### レビューの誤り

「DR-0027 の未確定は 9/9 の findings で回答済み」は不成立。残る未確定は
Bedrock/OpenAI 経路・ファイル上限・sub 既定。
