# 裁定・確認待ち一覧 (ユーザ用)

## 運用規約

<details>
<summary>ゼロコンテキストエージェント向け（本セクションは消さない）</summary>

- 裁定/確認待ち項目を 1項目=1ラベル=1セクション で記載
- ラベル形式: XX-Q1（XX は 2-3 文字、バッチやセッション内で一意、Qn単独の使い回し禁止、長期一意性は不要)
- 依頼形式: 「👺XX-Q1 の裁定お願いします」（参照用途ではラベルに👺を付けない。誤陽性がユーザのハイライト/アラームを汚す）
- チャット提示と同一ターンで本ファイルに記録 + path 指定 commit (push はリリース窓に同乗)
- 裁定が下りたら該当セクションを即削除し、内容は正規の記録先 (DR / issue / journal / close_reason) へ反映。本ファイルは常に「現在待ち」だけを持つ
- 参照は[]()で提示（リポ内は相対、リポ外はフルパス）
- 初版質問/依頼は長文で書かない（ユーザが説明を求めらたら本ファイルに説明を追加し、チャットで👺ラベルで再依頼）
- **選択肢・確認項目は `- [ ] a: …` 形式（チェックボックス + ラベル）で書く**。
  Q / C で記法を分けない。回答は「チェックを付ける」でも「XX-Q1a」と言葉で返すでも通る
  （複数まとめてチェックし「チェックしたよ」の一言で済ませる運用を想定）

</details>

## 裁定待ち

### 👺KA-Q1: DR-0027 (keepalive を自送信に置き換える) を Accepted にして実装に進めるか

- [ ] a (推奨): Accepted。実装 (保持層 = state dir のファイル、自送信 = 本文そのまま + `max_tokens: 1`、`origin: keepalive`、sub は設定で opt-in) に着手し、`keepalive-via-messaging-socket` は discard、`keepalive-daily-counters` は「ping 本数と read トークンを日別に」へ書き換えて吸収
- [ ] b: 見送り (Status を Rejected/Deferred に、理由を記録)。合図方式のまま運用継続

推奨理由: 実測で同一本文の replay が 5m / 1h とも TTL を更新すると確定 ([findings](./findings/2026-09-08-cache-ttl-refresh-on-hit.md))。合図方式の複雑さ (nonce / Foreign / 兄弟中継 / 起点別の hook / サスペンド後の盲目 ping) が丸ごと消え、sub の prefix も延命できる。[DR-0027](./decisions/DR-0027-keepalive-by-replay.md)。外部レビュー L-3 ([issue](./issue/2026-09-10-ecosystem-review-2026-09.md)) の blocked 解除条件。

### 👺KA-Q2: context 使用率通知 plugin の置き場

- [ ] a (推奨): 新規リポ `kawaz/claude-context-notify` (hook 3 本 + スクリプト + 7 段階文面 + README。依存なし、誰でも導入可)
- [ ] b: 既存 plugin に同梱 (`claude-rules-personal` の hooks / `ccmsg` plugin)

推奨理由: 要件が「hook だけで完結、誰でも導入できる」なので単機能の独立 plugin が素直。参照実装は [research](./research/2026-09-10-context-usage-notification.md) 付録に動作確認済み。`caffeinate` hook (スリープ抑止) も同じ形なので、同リポに 2 本目として置くか別リポかは a を選んだ時に併せて決める。

### 👺KA-Q3: claude-plugin-reference の未 push commit (7 件) を push / リリースするか

- [ ] a (推奨): push してリリース (CLI 新オプション 11 個 / stream-json 双方向 / `--permission-prompt-tool stdio` 等の公式挙動検証。非公式部分は llm-gateway research へ移設済み)
- [ ] b: 保留

推奨理由: 内容は公式挙動の実機検証で、hook 一覧の不足 (別 issue `hook-events-missing-from-reference`) とは独立。

## 確認待ち

（現在なし）
