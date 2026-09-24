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

### GA-Q4: `reference/delegation/model-effort-matrix` の残り 2 点 (GA-Q3 の a / b は裁定済み、編集して差分説明)

kawaz の指摘 (2026-09-24): opus を自走から外す根拠は無い (メインが手綱を取る worker なら心配不要)。統括も同意 (今日の opus-medium は多段実装を裁定どおり自走した。sol の利点は能力でなく費用と速度)。

- [ ] a: 既存の「不具合調査・デバッグ・原因の再現追跡 → sol-high」行を「sol-high / opus-high どちらも可。前提の裏取りが要る (仕様・パス・field 名を疑う) 場面は opus-high、試行回数で当てる場面は sol-high」に緩める
- [ ] b: メインの effort の注記は今は足さない。2026-10-01 頃の low / medium 比較 (memory に記録) の結果で決める (推し)
- [ ] c: メインの effort の注記を今足す (「medium 既定、途中で変えない」)

## 確認待ち

### PT-C1: 汎用パススルー (DR-0030 §2、分割の段 3) の対外仕様の具体形

DR-0030 §2 / §4 / §5 の裁定の範囲内で統括が確定し実装に進めた形。見て違和感があれば止めてほしい (無ければそのまま)。

```toml
[upstreams."api.x.ai"]      # 名前 = 既定は apifqdn、任意ラベル可。予約名: v1 / llm-gateway / llm
url = "https://api.x.ai"    # <rest> をそのまま連結
secret = "xai"              # 静的 secret の id (secrets/xai.json)
auth = "bearer"             # 載せ方: "bearer" / { header = "x-api-key" }。上流 API の形なので上流側に書く
allow = ["GET /v1/models", "POST /v1/chat/completions"]   # "METHOD path-pattern"、* は 1 個。外れは 404 / 405 で上流に出さない

[secrets]
type = "file"               # 既定の置き場 $XDG_STATE_HOME/llm-gateway/secrets/<id>.json。credential とはディレクトリを分ける
```

秘密ファイルの最小形は `{"type":"static","payload":{"value":"..."}}` (DR-0010 の版 + flock、読めなければ 502)。URL は `/ns-<ns>/<name>/<rest>` (ns 認証は LLM 経路と同じ)。クライアントの `Authorization` は必ず落とし、`Host` は上流に、他は本文・ヘッダ・応答 (SSE 含む) とも無変換。events に `passthrough` を 1 種追加 (`ns` / `upstream` / `method` / `path` / `status` / `duration_ms` / `secret`)。stats には積まない (回数は手順 3 のレート制限バケットで)。

- [ ] a: このまま (確認済み)
- [ ] b: 直してほしい点あり (本ファイルか ccmsg で)
