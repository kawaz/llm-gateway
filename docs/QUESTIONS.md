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

### GA-Q1: DR-0030 の URL 切替中、旧 URL `/ns-<ns>/v1/...` の一時 alias を持つか

背景: Caddy は `lb_policy first` で 11301 が落ちた時だけ 11302 に回り、404 では回らない (DR-0028、runbook 2026-09-09)。クライアントは 4 つ (Claude 設定 ×3 の `ANTHROPIC_BASE_URL` = `.../ns-<ns>`、codex の `base_url = .../ns-personal/v1`) で、走行中の Claude セッションは起動時の base URL を持ち続ける。unstable だけ先にパスを変えると、その瞬間から旧 URL は 404。

統括推し: **a**。alias は binary に持ち (旧 `/ns-<ns>/v1/<endpoint>` を今と同じ「model で route を選ぶ」扱いのまま残す)、**hit 数を `/llm-gateway/self` に出して 0 が続いたら消す**。Caddy 側の rewrite (c) は 127.0.0.1 直叩きに効かず、一斉切替 (b) は走行中セッションを止める。DR-0030 の「alias は残さない」(2026-09-17) はこれで置き換える。

- [ ] a: binary に一時 alias、`/self` の hit 数を見て削除 (推し)
- [ ] b: alias 無し。unstable / stable / 4 クライアント設定を同時に切替 (走行中セッションは再起動)
- [ ] c: Caddy の rewrite で旧 → 新 (127.0.0.1 直叩きには効かない)

### GA-Q2: `issued` (DR-0030 §6) の発行の口をどう設計するか

前提: refresh はアプリが自分で行うので **HTTP の token endpoint は必ず要る**。問いは「最初の 1 本 (bootstrap) をどこから出すか」と「endpoint の形」。

統括推し: **a**。bootstrap は host 上の CLI (`llm-gateway issue --ns <ns> --subject <app>` の類、署名鍵ファイルを直接読んで refresh token を標準出力に出す。gateway は保存しない = §6 の helper CLI と同じ姿勢。kid ごとの最終発行時刻だけ DR-0010 の flock 下で記録)。refresh は OAuth 2 の token endpoint の形 (`POST /ns-<ns>/auth/token`、`grant_type=refresh_token`) に合わせ、標準クライアントがそのまま使えるようにする。access の寿命と refresh の rotation は §未確定の方向どおり。

- [ ] a: CLI で bootstrap (refresh を発行) + HTTP token endpoint で refresh → access (推し)
- [ ] b: HTTP だけ。bootstrap も HTTP (admin 用の固定 token で叩く)。gateway に admin 認証がもう 1 系統増える
- [ ] c: CLI だけ。access も CLI で出す (短命 access を人手で配り直す運用になり、アプリの自動 refresh が無い)

### SL-Q1: DR-0031 (Store 層) の裁定

[DR-0031](decisions/DR-0031-store-layer.md) — 永続化を 4 つの一貫性の意味論 (単一 writer の更新 / リース / 合算可能なカウンタ / LWW スナップショット) で trait に切り、file backend を 1 実装にする。issue の合意 + harnessrouter からの補強 (fail-closed / best-effort を契約に、リースに fencing token) + DR-0030 §5 の裁定を統合。worker 起草 → reviewer-sol-high で 2 回検査 → 統括が通し読み済み。

統括推し: **a**。要点は (i) trait は意味論で切り名前と signature は段 1 で確定、(ii) fencing token が守るのは store への書き込みだけで upstream 送信は claim + `fires_at_ms` の検出のまま、(iii) レート制限バケットの write / read は fail-closed、stats は best-effort、(iv) LWW は目標の契約で file backend の段 1 は挙動不変、(v) 分割の段 1 で (1) だけ切り、(2)(4) は 2 つ目の backend を入れる時。

- [ ] a: Accepted (⬜ 未実装) にして gateway-core 分割の段 1 に進む (推し)
- [ ] b: 修正して再提出 (指摘は本ファイルか ccmsg で)
- [ ] c: 保留 (Store 層自体を今は切らない)

### GA-Q3: `reference/delegation/model-effort-matrix` の更新案 (claude-rules-personal、rules 変更なので裁定後に編集)

2026-09-23 の実測 (llm-gateway 統括): Opus 5.5 high ×1 (原因分析、一発で正確・事実と推測の分離・副作用列挙)、Opus 5.5 medium ×5 (実装、全て裁定どおり・指示外の妥当な懸念を自発報告・追加の対称修正)、gpt-6-sol ×3 (hook 修正・probe・真因調査。速いが前提を裏取りしない: reference の誤記を信じて hook を無効化、namespace のパス形式未確認で 4 本無駄撃ち、テストを実装に合わせて通す)。費用 (stats、req あたり): Opus 5.5 は Opus 5 の約半分 (0.049 vs 0.094 USD)、gpt-6-sol は 5.6-sol の約 1/3 (0.144 vs 0.399 USD)。

- [ ] a: 「opus: 特性は評価中」→「Opus 5.5 は medium でも検証が厚く、指示外の妥当な懸念を自発報告する。req あたり費用は Opus 5 の約半分」
- [ ] b: 「codex (sol): 評価は実務で更新する」→「gpt-6-sol は 5.6 比で約 1/3 の費用。前提 (仕様・パス・field 名) を裏取りせず進む傾向があるので、委譲プロンプトに一次資料と検証手順を焼き込む。実装より probe / 修正の自走向き」
- [ ] c: 「不具合調査・デバッグ → sol-high」の行を「原因分析は opus-high、再現追跡・自走は sol-high」に分ける
- [ ] d: メインの effort に関する注記を足す: 「メインは履歴の cache read がコストの主成分で effort を下げても半減しない。判断ミスは worker のやり直しで跳ね返るので medium 既定・大型 high、低くしない。途中で変えない (messages cache が無効化される)」

## 確認待ち

（現在なし）
