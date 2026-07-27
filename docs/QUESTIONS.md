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

### 👺GW-Q1: Bedrock 経路で `anthropic-beta` をどう扱うか

実測: クライアントが送る beta 束をそのまま透過すると Bedrock は 400。
10 個中 5 個 (`oauth-2025-04-20` / `prompt-caching-scope-2026-01-05` /
`fast-mode-2026-02-01` / `redact-thinking-2026-02-12` /
`token-efficient-tools-2026-03-28`) を拒否する。beta 無しなら 200。

- [ ] a (推奨): **拒否リストを持ち、Bedrock 向けにはそれだけ落として残りを透過**
- [ ] b: Bedrock 向けは `anthropic-beta` を丸ごと落とす
- [ ] c: 許可リストを設定に持ち、列挙されたものだけ通す (cpa の現行方式)

a を推す理由: 拒否されるフラグだけを外すので、受理される機能
(`context-1m` / `context-management` / `interleaved-thinking` /
`structured-outputs` / `claude-code`) が落ちない。cpa は c 方式で「束ごと置換」した
結果 `context-management` を落として実障害を出した (llm-notes DR-0001)。
b は安全側だが 5 機能を捨てる。

懸念: 拒否リストはハードコードだと upstream 変更に追従できない。
設定ファイルで上書き可能にした上で、既定値をコードに持つのが妥当と考える。

### 👺GW-Q2: v1 の配布形態と常駐方法

- [ ] a (推奨): cpa と同じ launchd + ラッパスクリプト方式 (`~/Library/LaunchAgents/com.kawaz.llm-gateway-<face>.plist`)
- [ ] b: cache-warden と同じ `.app` bundle 方式
- [ ] c: 常駐させず foreground 実行のみ (v1 は手動起動)

a を推す理由: cpa からの移行なので同じ運用形態が乗り換えやすい。
b の `.app` は TCC 権限 (TouchID 等) が要る cache-warden 固有の事情によるもので、
llm-gateway には不要。c は移行判定に日単位の常用が要るので不便。

## 確認待ち

### 👺GW-C1: 秘密の保存先とファイル形式

- [ ] a: 保存先は `~/.cache/llm-gateway/auth/` (XDG_CACHE_HOME 配下)
- [ ] b: ファイル形式は cpa 互換 JSON (`{type, email, access_token, refresh_token, expired, last_refresh, priority, disabled, excluded-models}`)
- [ ] c: 初期投入は cpa の `auth-personal/*.json` からのコピー (元ファイルは触らない = cpa と併存)
- [ ] d: バックアップ済み `~/.cache/llm-gateway/auth-backup/20260727T190012/` (4 ファイル、chmod 600)

「おいおいファイル保存から脱却するまでは気にしなくて良い」との指示なので
このまま進める前提。認識違いがあれば指摘してほしい。

### 👺GW-C2: Phase 1 のスコープ (gpt は cpa へ転送)

- [ ] a: Phase 1 は Claude 系 (OAuth プール + Bedrock) のみ自前実装
- [ ] b: `gpt-*` は設定で cpa (8317) へ丸ごと転送する `PassthroughProxy` バックエンド
- [ ] c: Phase 2 で Anthropic⇄Responses 変換 (cpa 実装で本体 1,758 行) を自前化して cpa 依存を切る

b の副作用: cpa を残す間、sol 経路には cpa の beta 注入問題が残る
(現状 実害なし)。「cpa を完全に止める」のは c の完了後になる。
