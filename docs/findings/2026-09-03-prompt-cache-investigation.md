# prompt cache 調査 (keepalive 設計の根拠と再現手順)

DR-0024 / knowledge/2026-09-02-prompt-cache-and-thinking-facts.md の根拠となった
調査の、数値サマリと再現手順。確定事実そのものは knowledge が正本。

## 判明した事実

- 過去 7 日・142 セッション (main 53 / subagent 90 前後) の費用構成 (Fable 5.1 単価換算):
  合計 ≈ $6.4K、うち write $4.6K / read $1.3K。write の 88% は 5 分 TTL 失効後の
  全プレフィックス再構築 (792 回) で、その 66% が失効から 60 分以内の再開
- 費用の 9 割は main セッション。subagent は連続実行で read 再利用が効き
  rebuild が少ない (write の 55%) ため、cache 戦略の介入は main だけで足りる
- idle gap (≥60 分) は 7 日で約 245 回。horizon シミュレーション (1〜96h) の
  最適値は全体 6h、5.1 系のみなら 15h — いずれも損益分岐時間の 0.2〜0.35 倍に
  収まるため、設定は比率 (keepalive_horizon = 0.3) で持つのが自然
- 損益分岐: ping 1 回 = prefix × read 単価、rebuild = prefix × input × 2.0。
  分岐回数 = 両者の比で **prefix サイズに依存しない** (5.1 系 80 回 ≈ 73h、
  他モデル 20 回 ≈ 18h。read 0.1 倍 / write 2.0 倍の比が全モデル共通のため)

## 実用的な示唆 / ベストプラクティス

- 集計は `scripts/cache-cost-sim.py` / `scripts/keepalive-horizon-sim.py` を
  再実行すれば最新化できる (レポートはリポ外 `~/.cache/claude-session-state/llm-gateway/`
  に出る — 業務リポの cwd を含むためリポに入れない)
- セッション jsonl の usage 解析では **同一 `message.id` の行が複数回出る**
  (streaming 途中の中間値)。dedupe して最後の値を採ること (未 dedupe だと
  リクエスト数もトークンも約 2 倍に膨れる)
- 本文レベルの観測は tap (`?include=request_body&max_body=…`) で。捕捉 dump は
  会話本文を含むので解析後に必ず削除
- gateway 経由で合成プローブを送る時は Claude Code の形が必須
  (issue/2026-09-03-oauth-requires-claude-code-shape.md)。最小形:
  system[0] に `x-anthropic-billing-header: …`、system[1] に
  `You are Claude Code…`、`metadata.user_id`、UA `claude-cli/…`、`x-app: cli`、
  beta ヘッダ。欠けると 429 `{"message":"Error"}`
- 拒否応答 (`stop_reason: refusal`) はキャッシュを残さないので、プローブ本文は
  自然な文章にする (乱数語の羅列は classifier に弾かれる)
- TTL 昇格実験の型 (E 系列): 5m で書く → 新ターン + 全 1h → usage の
  `cache_creation.ephemeral_{5m,1h}_input_tokens` で read/write の内訳を見る。
  数分・数ドルで決着する

## 検証の詳細

- 集計・シミュレーションの全数値: リポ外レポート (上記) と各スクリプトの出力
- 合成実験の usage ログ・実機 keepalive の観測ログ: 本セッション
  (2026-09-02〜03) にて。要旨は knowledge に転記済み
