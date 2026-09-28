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
- **選択肢・確認項目は `- [ ] a: …` 形式（チェックボックス + ラベル）で書く**。Q / C で記法を分けない。回答は「チェックを付ける」でも「XX-Q1a」と言葉で返すでも通る（複数まとめてチェックし「チェックしたよ」の一言で済ませる運用を想定）

</details>

## 裁定待ち

（現在なし）

## 確認待ち

### JW-C1: Claude Code 向けを固定 token から長寿命 JWT に切り替える (裁定済み a、進行中)

kawaz 裁定 (2026-09-25): 統括が [runbook](runbooks/ns-auth-jwt-rotation.md) どおりに進めてよい (config と settings.json の編集を含む、順序は unstable → personal → emrd → zunsystem)。

制約: 認証方式は ns 単位で `token` か `jwt` のどちらか一方なので、ns を切り替えた瞬間にその ns を使う走行中セッション (personal は llm-gateway 統括と codex) は再起動まで 401 になる (Caddy は 401 で fail over しない)。手順は (1) 鍵と JWT と settings.json の準備 (無停止) → (2) ns ごとに config 切替 + そのセッションの再起動。

- [ ] a: (2) の personal の切替をこのセッションを切る合図と同時に行う (統括推し)
- [ ] b: (1) だけ先に済ませ、(2) は kawaz が合図する別のタイミングで

JW-C1β: (1) の着手に要る秘密鍵 (`auth keygen` の JWK、機械ごと 1 本) の置き場。runbook は「パスワードマネージャに移してファイルは消す」とだけ言う。

- [ ] a: 1Password の personal vault に item を作り、`auth sign` は `op run` 経由で `op://` 参照から読む (統括推し。`secret-hygiene` rule の透過運用そのまま、AI が秘密鍵の値を見ない)
- [ ] b: `~/.config/llm-gateway/private/` (gitignore 下、chmod 600) に置く

JW-C1γ: 監督者の 0.59.2 化 (`llm-gateway version` は `supervisor.running = 0.56.0`、`restart_needed = true`、動作に支障なし)。`service stop` は抱えている stable / unstable も落とすので、gateway 経由の全セッション (この統括を含む) の走行中 request が切れる。(2) の personal 切替と同じ再起動の窓で一緒にやる想定。別の窓が良ければ指示を。
