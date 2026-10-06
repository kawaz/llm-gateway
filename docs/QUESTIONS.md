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

### IS-Q2 `/low-priority` の slow 付き 429 / 529

[low-priority-slow-requests-should-wait-not-switch](issue/2026-09-25-low-priority-slow-requests-should-wait-not-switch.md)。今は DR-0009 で別 credential へ切り替えるので、待てば通る 429 で別 subscription の枠を消費しうる。

- [ ] a (推し): `anthropic-usage-limit: slow` 付きは DR-0009 の例外にして、429 `slot_busy` / 529 を切替も cooldown もせず透過する。Claude Code 側の待機に任せる。本人が週枠の前借りを選んでいるので pace_cap の対象からも外す
- [ ] b: 切替はするが、全滅時に slow ヘッダ付きの最後の応答を必ず透過する
- [ ] c: 実機で `/low-priority` が gateway 越しに効くか (送信条件) を先に確かめてから決める

### IS-Q3 daemon restart --all の順序と停止猶予

[daemon-restart-order-and-grace](issue/2026-09-09-daemon-restart-order-and-grace.md)。猶予の SIGKILL は v0.44.4 で解決済み。残りは順序 (`priority`) と猶予を設定可能にするか。

- [ ] a (推し): 両方やらずに close。unit 2 台で、登録の逆順で困ったのは 9/09 の 1 回だけ。猶予は固定 5s で SIGKILL が出なくなった
- [ ] b: `priority` だけ入れる (`daemon add --priority`、`daemon list` に表示)
- [ ] c: `priority` と `stop_grace_secs` の両方を入れる

### IS-Q4 監督者のログをローテーション後に開き直す

[supervisor-log-reopen-on-rotation](issue/2026-09-10-supervisor-log-reopen-on-rotation.md)。今は次の restart まで旧ファイルに書き続ける (壊れはしない)。

- [ ] a (推し): 書き先の inode 変化を監督者が検知して開き直す。newsyslog の設定 (`N` フラグ) を変えずに済み、logrotate でも同じに効く
- [ ] b: SIGHUP で開き直す (newsyslog から `N` を外し pid ファイルで合図)
- [ ] c: やらずに close (runbook の「次の restart から効く」で足りる)

### IS-Q5 サブスク OAuth の probe を Claude Code の形に揃える範囲

[oauth-requires-claude-code-shape](issue/2026-09-03-oauth-requires-claude-code-shape.md)。usage の probe (`preset/anthropic/quota_api.rs`) は User-Agent / x-app / `metadata.user_id` 等が無く、上流が「Claude Code の形でない」と弾く形のまま。

- [ ] a (推し): probe を Claude Code の形に揃えるだけ先にやる。「429 `Error` を即 4xx 扱い」は DR-0009 の例外になるので、弾かれる実害を確かめてから別に決める
- [ ] b: 両方まとめてやる
- [ ] c: 実害 (probe が弾かれているか) を確かめてから決める

### IS-Q6 Sonnet 5.5 の issue に残る「linked account の対応を設定で表す」

[sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it](issue/2026-09-30-sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it.md)。本体は DR-0035 で実装済み。受け入れ条件のこの 1 項目だけ機構が無い。

- [ ] a (推し): 不要として close。DR-0035 は account をそのまま束縛の単位にしていて、linked の対応表を要する場面が出ていない
- [ ] b: 後続 issue に切り出して close

## 確認待ち

（現在なし）
