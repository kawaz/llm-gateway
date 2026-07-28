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

### AUTH-Q1: default namespace が認証なしで外から叩ける

config に `auth_token` があるのは `ns.personal` と `ns.emrd` だけで、**default namespace には認証がない**。
Caddy が `llm-gateway.kawaz-mbp16-20211217.kawaz.jp` として公開しているため、
**tailnet 上の任意のデバイスから認証なしで `/v1/messages` を叩ける**。

canddy 側のセッションが実機で確認済み (2026-07-28 15:10Z)。Authorization ヘッダを一切付けずに
POST して 200 が返り、**実際に課金が発生した** (haiku, input 19 / output 11 tokens)。
tailnet の参加デバイスは 8 台で、うち複数が**クラウド上の Linux ホスト** (公開 IP を持つ)。

**注意**: gateway 側で default に `auth_token` を設定すると、Caddy の active health check
(`uri: /v1/models`, `expect_status: 200`, 5 秒間隔) が 401 を受けて unhealthy 判定になり、
**8402 / 8401 とも落ちて全断する**。単独では打てない。

- [ ] a: Caddy 側で応急処置 (`/ns-` で始まるパスのみ通す) → その後 gateway に `/healthz` を新設し、
      health check を切り替えてから default に `auth_token` を設定する (推し。断が無い順序)
- [ ] b: gateway の `/healthz` 新設を先にやり、Caddy 側の応急処置は挟まない (穴が開いている時間が延びる)
- [ ] c: Caddy の llm-gateway route を閉じる (最も確実。ただし `settings.json` がこの URL を指しているので、
      BASE_URL を `http://127.0.0.1:8402` へ戻す作業とセットでないと稼働中セッションが止まる)
- [ ] d: 現状のままにする (tailnet 内は信頼できる前提を維持する)

### AUTH-Q2: `/healthz` を新設してよいか

AUTH-Q1 の a / b を選ぶ場合に必要。現在 health check に使われている `/v1/models` は
credential ごとのモデル一覧を返す実質的な業務エンドポイントで、「プロセスが生きているか」を
見るには重い。認証も credential も要らない軽量なエンドポイントを分けたい。

- [ ] a: 新設する (推し)
- [ ] b: 新設せず、Caddy の health check に認証ヘッダを載せる方向で解く

## 確認待ち

（現在なし）
