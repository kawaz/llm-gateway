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

open issue の消化 (2026-10-06) で、着手前に方針の一言が要るもの。推しは各項目の先頭。

### IS-Q1 service re-register で XDG の値が変わった時の扱い

[service-reregister-env-drift-not-detected](issue/2026-10-04-service-reregister-env-drift-not-detected.md)。`service register` は XDG を plist に固定するが、re-register で値が変わっても黙って書き換わる。

- [ ] a (推し): 値が変わる時は差分を出して止め、`--force` で通す。reference `cli-daemon-subcommands` の「意図しない置き換えは止める」に沿う。daemon 側の検知 (名乗る場所と導出の比較) は別 PR
- [ ] b: 黙って書き換える現状のまま、warning だけ出す

### IS-Q6 Sonnet 5.5 の issue に残る「linked account の対応を設定で表す」

[sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it](issue/2026-09-30-sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it.md)。本体は DR-0035 で実装済み。受け入れ条件のこの 1 項目だけ機構が無い。

- [x] a (推し): 不要として close。DR-0035 は account をそのまま束縛の単位にしていて、linked の対応表を要する場面が出ていない
- [ ] b: 後続 issue に切り出して close

### IS-Q7 DR-0035 の改定: thinking の出所を session でなく「連なり (session + subagent の agent id)」で持つ

[DR-0035](decisions/DR-0035-judge-thinking-crossing-by-session-sources.md) 決定 2。subagent は main と同じ session を名乗るので、別モデルの subagent が main を「跨いだ」扱いにし、`thinking_as_text` の ns では main の thinking が毎回 text になっていた (実測)。実装済み (70be6bc) で、DR の Status は「改定は kawaz の裁定待ち」。

- [ ] a (推し): 改定を承認。subagent の履歴は main と別で、結果は tool_result の text として戻るので、thinking の連なりは agent ごとに分かれるのが実態どおり。agent id を名乗らない 1 本は main に入れる
- [ ] b: 承認するが、親の履歴を引き継ぐ fork の subagent で親の thinking が黙って落ちる端 (DR の「既知の端」) を先に潰す方法を検討する
- [ ] c: 差し戻す

## 確認待ち

（現在なし）
