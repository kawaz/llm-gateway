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

### LM-Q1 長い busy の締め出しも 1 時間ごとの枠照会の対象にするか

窓を伴わない 429 (Fable の週の枠切れ等) は `retry-after` で数日先までの `busy` になる。`claim_probe` は `limited` の締め出しにしか定期の枠照会を立てないので、`limited` の印が無い状況 (その場の照会が失敗した、照会直後に枠が尽きた) では外でリセットされても `retry-after` まで残る。枠照会はトークンを使わない。DR-0009 の「定期の様子見は上限で締め出している経路にだけ立てる」の変更になる。関係: [denial.rs の claim_probe](../crates/llm-gateway/src/denial.rs)

- [ ] a: 残り期限が `PROBE_INTERVAL` (1 時間) より先の `busy` も定期照会の対象にする (統括推し。増えるのはトークンを使わない照会が 1 時間に 1 回)
- [ ] b: 今のまま (再起動か日曜で解ける)

### LM-Q2 components で絞っている source で、紐づきの無い incident (scope=page) を表示するか

OpenAI の incident は components を持たないので、`components = ["Responses"]` で絞ると incident が全部落ちて 1 件も出ない。DR-0021 は「参考表示できる、severity は上げない」としか決めていない。

- [ ] a: 絞り込み中でも `scope = "page"` の incident は表示する (統括推し。severity は components から決まるので警告レベルは上がらない。Codex と無関係な障害も並ぶ)
- [ ] b: 今のまま (絞り込み中は紐づきの無い incident を出さない)

### LM-Q3 DR-0043 の「署名を求めない口」から未実装の hooks/jwks を書き分けるか

DR-0043 決定 2 と INDEX が `GET /llm-gateway/hooks/jwks` (DR-0041、Proposed で router に無い) を署名を求めない口として挙げている。

- [ ] a: 「DR-0041 を実装したら hooks/jwks もこちら」と書き分ける (統括推し)
- [ ] b: 決定 2 は変えず、DR 末尾に「hooks/jwks は DR-0041 (Proposed) の口で現時点で router に無い」の 1 行だけ足す

### LM-Q4 usage の型名と JSON 欄名を「経路」に合わせるか

`/llm-gateway/usage` の 1 件は経路 (`[routes.<name>]`) ごとだが、型は `CredentialUsage`、JSON の欄は `credentials`。コメントと MANUAL は「経路ごと」に直してある。

- [ ] a: 型名と JSON 欄名を経路の語に変える (読む側 = ccmsg 等の追従が要る)
- [ ] b: 型名だけ変え、JSON 欄名は据え置く
- [ ] c: 今のまま

### LM-Q5 締め出し中に subscription_status が active のとき、hint からサブスクリプションの話を外すか

組織ごと断られた経路の `auth` は、`past_due` 以外では既定の hint (「an inactive subscription is one cause」) を出す。`subscription_status: active` と並ぶと、状態は有効なのに hint が停止を示唆する形になる。

- [ ] a: `active` のときは hint からサブスクリプションの話を外す (統括推し。原因が契約の外にあると分かっているので)
- [ ] b: 今のまま

### DS-Q3 監督者が落ちた後に残った子を、次の監督者がどう止めるか

対象は kawaz 製 CLI 共通の標準 (正本は claude-rules-personal の reference `cli-design/daemon-subcommands`)。直す案と、決まった点・統括が埋めた点の一覧は [/Users/kawaz/.local/share/repos/github.com/kawaz/llm-gateway/artifacts/cli-daemon-service-draft/](/Users/kawaz/.local/share/repos/github.com/kawaz/llm-gateway/artifacts/cli-daemon-service-draft/) の `daemon-subcommands.md` と `NOTES.md`。

方針は決まった (kawaz 2026-11-14): 監督者がやるのは死活の監視だけ。残った子は役目を続け、次の監督者が気づいて、前のを止めて起こし直すか、情報を出す。unit が他の unit を知るために監督者へ問い合わせる設計は禁じる。残るのは止め方。llm-gateway の DR-0028 決定 11 は「引き取らない、人が畳む」で、理由は pid が使い回されて自分の子だと確かめられないこと。今の unit の制御口は停止の頼みを受けない (設定の読み直しと一時ダンプだけ)。

- [ ] a: unit の制御口に停止を頼んで引き継ぐ。pid は使わない (制御口に答えるのは、その置き場を握っている本人だけ)。unit の制御口が停止の頼みを受けることを必須にする。答えない時は触らず、status に出す (統括推し。案はこの形で書いてある)
- [ ] b: 次の監督者は止めない。status に出すだけにして、人が止める (今の llm-gateway の決まりのまま)

## 確認待ち

（現在なし）
