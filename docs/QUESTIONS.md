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

**更新 (2026-07-29)**: 応急処置の案が 1 つ潰れた。canddy 側の実測で、
`/ns-` で始まるパスに絞っても **`/ns-default` が一致するので迂回できる**ことが判明。
代わりに **auth_token を持つ namespace だけを明示列挙するホワイトリスト**
(`/ns-personal/*` と `/ns-emrd/*` のみ) にする。認証なしの namespace を新設しても
穴にならない fail-safe な形。

`/healthz` は実装・push 済み (AUTH-Q2 は実質決着)。ただし**稼働プロセスには未反映**なので、
health check の切り替えはまだできない (切り替えると 404 で全断する。canddy 側が実測して発見)。

- [ ] a: Caddy 側でホワイトリスト → `just install` で反映 → canddy が `/healthz` の 200 を実測確認 →
      health check を切り替え → default に `auth_token` を設定 (推し。断が無い順序)
- [ ] b: Caddy 側の応急処置を挟まず、反映と切り替えだけで進める (穴が開いている時間が延びる)
- [ ] c: Caddy の llm-gateway route を閉じる (最も確実。ただし `settings.json` がこの URL を指しているので、
      BASE_URL を `http://127.0.0.1:8402` へ戻す作業とセットでないと稼働中セッションが止まる)
- [ ] d: 現状のままにする (tailnet 内は信頼できる前提を維持する)

### AUTH-Q3: `auth_token` 未設定の namespace を「誰でも通す」ままにするか

`Namespace::accepts()` は `auth_token` が未設定なら無条件で true を返す。

> 設定に書いていなければ誰でも通す。127.0.0.1 で待ち受けている前提で、
> 同じマシンの他プロセスと区別したいときだけ書く。

この前提は前段に Caddy を置いた時点で崩れた。AUTH-Q1 の穴はこの fail-open が
**設定ファイルの外で起きた変化** (公開経路の追加) によって顕在化したもの。

[DR-0006](./decisions/DR-0006-namespace-routing.md) は既定 namespace の特別扱いを外すが、
`accepts()` の挙動自体は変えていない。**名前付き namespace で書き忘れれば同じ穴が開く。**

- [ ] a: fail-closed にする (`auth_token` 未設定の namespace は拒む)。
      ただし全 config に `auth_token` を書く必要が生じ、ローカル専用の使い方が面倒になる
- [ ] b: 「認証なしを許す」を明示的に書かせる (例: `auth_token = "none"` のような明示)。
      書き忘れは拒み、意図した無認証は通す
- [ ] c: 現状のまま (未設定 = 誰でも通す)。DR-0006 の deny-all で個別に塞ぐ

## 確認待ち

（現在なし）
