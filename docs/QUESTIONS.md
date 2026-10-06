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

### IS-Q6 Sonnet 5.5 の issue に残る「linked account の対応を設定で表す」

[sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it](issue/2026-09-30-sonnet-5-5-thinking-is-account-bound-so-route-switches-drop-it.md)。本体は DR-0035 で実装済み。受け入れ条件のこの 1 項目だけ機構が無い。

- [x] a (推し): 不要として close。DR-0035 は account をそのまま束縛の単位にしていて、linked の対応表を要する場面が出ていない
- [ ] b: 後続 issue に切り出して close

### IS-Q8 OAuth probe の issue は前提が崩れていた。どう畳むか

[oauth-requires-claude-code-shape](issue/2026-09-03-oauth-requires-claude-code-shape.md)。IS-Q5a で着手する前に現状を確かめた (読み取りのみ、上流へは送っていない):

- ログ (unstable 7/28〜、stable) に、形を理由に弾かれた 429 (本文が素の `"Error"`) は 0 件。probe の失敗 24 件は refresh token 切れ・組織の OAuth 不許可 (403)・接続失敗・本物の rate limit (429 `would exceed your account's rate limit`) だけ
- usage は別経路の fetch (`/api/oauth/usage`) で 293 回取れており、実トラフィックの応答ヘッダでも更新されている
- 9/03 に見た「小さい probe だけ 429」は手で作った合成リクエストでの観測で、gateway の probe が弾かれた記録は無い
- Claude Code の形 (User-Agent / x-app / 請求ヘッダ / `metadata.user_id`) は本文の調査から変わっていない (2.1.289〜2.1.291 の控えで確認)
- 気づき: クライアントは `anthropic-beta` に `oauth-2025-04-20` を入れておらず、gateway も足していないのに実トラフィックは 200 で通っている。`quota_api.rs` の「載せないとサブスクの認証が通らない」というコメントの前提が怪しい (未検証)

着手前に書いていた probe の差分は戻した (未コミットだった)。

- [ ] a (推し): 「gateway の probe では観測されていない」として close する。実害が出たら、本文の調査とこの節の観測を起点に起票し直す。beta のコメントの真偽は、次に `quota_api.rs` を触る時に確かめる
- [ ] b: open のまま残し、probe の成功もログに出すようにして観測を続ける
- [ ] c: 上流に probe を 1 本送って (credential を使う外向きの送信)、今の形と oauth beta の要否を実機で確かめてから決める

## 確認待ち

（現在なし）
