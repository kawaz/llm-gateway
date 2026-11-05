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

### CA-Q1 DR-0042 / DR-0043 を Accepted にして実装に入る

[DR-0042](decisions/DR-0042-session-keepalive-state-endpoint.md) / [DR-0043](decisions/DR-0043-authenticate-operations-callers-by-their-jwks.md)。11-04/05 の裁定 (pull 専用、呼び手の JWKS、`exp` 10 秒主体) は反映済み。偵察で見つかった DR と実コードの食い違いのうち、裁定が要るのは CA-Q2〜Q4 で、残り (ずれ許容の定数を分ける、`Reason` の enum を新しく持つ、`cache_notice` の無い古い控えは `null`、`iat` が付いていれば無視、events の ACAO はそのまま) は統括が DR の「起草時に決めた」に追記して進める。

- [ ] a: Accepted (Status と INDEX を ⬜ に変えて実装開始)
- [ ] b: 待て (理由を聞く)

### CA-Q2 401 / 400 の `message` に落ちた段の語を出すか (DR-0043 決定 4)

DR-0043 は `message` に `unknown_kid` / `expired` 等を書くと決めているが、既存の ns 認証は「理由はログだけ、応答では区別しない (kid の有無や期限切れを外から探らせない)」([jwt.rs](../crates/gateway-core/src/ns/jwt.rs) 冒頭、[lib.rs `denied`](../crates/llm-gateway-server/src/lib.rs))。sessions は Caddy 越しに外へ通す口なので、既存方針の根拠がそのまま当たる。統括推しは a: 呼び手 (ccmsg) の運用者と gateway の運用者は同じで、段はログで読める。401 (署名し直せば通りうる) / 400 (直しても通らない) の分け方だけで呼び手の再送判断には足りる。

- [ ] a: 既存方針に揃える (`message` は固定文、段はログ) ← 推し
- [ ] b: DR のまま (段の語を `message` に出す)

### CA-Q3 `jwks_file` の相対パスの基準 (DR-0043 決定 1)

DR は「設定ファイルの置き場から (`keys_file` と同じ)」と書くが、実際の `keys_file` は `~` と環境変数を開くだけで ([config.rs `path_expand::serde_opt_path`](../crates/llm-gateway/src/config.rs))、相対パスはプロセスの cwd 基準になる。統括推しは a: daemon の cwd は不定なので設定ファイル基準が正しく、`keys_file` も同じ扱いに直す (`~` / 絶対パスの既存設定は変わらない)。

- [ ] a: 両方とも設定ファイルの置き場基準にする (`keys_file` も直す) ← 推し
- [ ] b: 両方とも今の `keys_file` と同じ (`~` 展開のみ、相対は cwd) にして DR を直す

### CA-Q4 手動確認の道具 (DR-0043 未決、実装の第 6 段で要る)

- [ ] a: `llm-gateway call <METHOD> <path> [--caller <name>]` を足す (署名して問い、答えを出す。`usage` / `stats` の署名経路を共用、`auth sign` は ns 専用のまま) ← 推し
- [ ] b: `auth sign` に `--typ` を足し `--sub` を任意にする (命令は増えないが ns の道具に運用系の形が混ざる)

## 確認待ち

（現在なし）
