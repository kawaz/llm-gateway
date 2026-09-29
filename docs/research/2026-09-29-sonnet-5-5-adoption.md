# Claude Sonnet 5.5 導入調査

- Date: 2026-09-29
- Status: In Progress

## 動機と調査範囲

Sonnet 5.5 の公式価格・性能・モデル ID と、llm-gateway およびローカルのモデル設定の変更候補を特定する。ここでは設定・実装を変更せず、公式資料と現行ファイルを照合する。

## 調査メモ

### 2026-09-29: 公式仕様と価格

[公式モデルページ](https://platform.claude.com/docs/en/models/sonnet-5-5/overview)によると、公開日は 2026-09-28、Claude API の ID は `claude-sonnet-5-5`、context は 1M tokens、通常の最大出力は 128K tokens、Batch API は beta header を付けて最大 300K tokens。Anthropic の[モデル ID 規則](https://platform.claude.com/docs/en/about-claude/models/model-ids-and-versions)では 4.6 以降の日付なし ID が固定スナップショットであり、5.5 の日付付き ID は存在しない。モデル一覧の「alias」欄にも同じ `claude-sonnet-5-5` が記載されるが、以前の世代の可変 alias とは異なる。[価格資料](https://platform.claude.com/docs/en/about-claude/pricing)では 4.6 以降の 1M context は全域が標準単価であり、200K 超の別料金はない。

| モデル | Input | Output | 5m cache write | 1h cache write | Cache read | Batch input/output |
|---|---:|---:|---:|---:|---:|---:|
| Sonnet 5.5 | $2 | $10 | $2.50 | $4 | $0.20 | $1 / $5 |
| Sonnet 5 | $2 | $10 | $2.50 | $4 | $0.20 | $1 / $5 |
| Opus 5.5 | $4 | $20 | $5 | $8 | $0.20 | $2 / $10 |
| Fable 5.1 | $10 | $50 | $12.50 | $20 | $0.25 | $5 / $25 |

単位は USD / 100 万 tokens。表は[公式価格表](https://platform.claude.com/docs/en/about-claude/pricing)の Claude API 標準単価。Batch は入出力とも 50% 割引。Sonnet 5 の $2/$10 は導入期限後も標準価格として確定し、予定されていた $3/$15 への値上げは実施されなかった。

### 2026-09-29: 性能・移行時の挙動

[Anthropic 発表](https://www.anthropic.com/claude-sonnet-5-5)の公表値では、Sonnet 5.5 / Sonnet 5 / Opus 5.5 の順に Terminal-Bench 4.0 が 70.6% / 10.3% / 66.4%（Opus は xhigh）、CursorBench 4.0 が 55.5% / 34.1% / 57.8%、OSWorld 2.1 partial が 80.1% / 57.0% / 81.8%。FrontierCode 1.1 Main は Sonnet 5.5 max 46.2%、xhigh 52.1%、Sonnet 5 42.4%、Opus 5.5 54.4%。測定の effort や条件が行・モデルごとに異なるため、同条件の実測比較とは読み替えない。Anthropic は Sonnet 5 比で生成速度が 30% 以上速く、トークン効率改善によりタスク単位の費用が最大 30% 低いと説明するが、ここでは自環境の追試をしていない。公式モデル一覧は相対的な速度を Sonnet 5.5 = Fast、Opus 5.5 = Moderate、Fable 5.1 = Slower と表現する。

[移行差分](https://platform.claude.com/docs/en/models/sonnet-5-5/whats-new-sonnet-5-5)には `thinking: disabled` が 400（代わりに `between_tools`）、強制 `tool_choice` (`any`/`tool`) が 400、thinking block のモデル・会話への結合、旧 `computer_20251124` 非対応、advisor の組み合わせ制限がある。ツール間の長めの発言は既定で内容を省略した thinking block となる。Sonnet 5 と tokenizer は同じ。effort の度合いが再調整されており、同じ設定を移すだけで品質・費用が同じとは限らない。

### 2026-09-29: Claude Code と gateway

[Claude Code changelog](https://code.claude.com/docs/en/changelog)は v2.1.284 で Sonnet 5.5 対応と Anthropic API 上の既定 Sonnet への採用を記す。[モデル設定資料](https://code.claude.com/docs/en/model-config)では `claude --model claude-sonnet-5-5` / `--model sonnet` が指定可能で、`sonnet` の実際の既定は provider に依存する。`ANTHROPIC_DEFAULT_SONNET_MODEL` は `sonnet` / `opusplan` 実行フェーズの解決先を固定する設定であり、ここでは 3 環境とも `claude-sonnet-5[1m]` が明示されているため、Claude Code 本体の既定更新だけでは切り替わらない。Anthropic API では Sonnet 5.5 の 1M context はネイティブだが、gateway の `ANTHROPIC_BASE_URL` 越しには 1M 対応を検出できず 200K として予算計上する場合があり、`sonnet[1m]` / モデル選択画面の 1M 選択が意味を持つ。`claude --version` の実測値は 2.1.284。gateway 経由で `claude-sonnet-5-5[1m]` が実際に解決するかは未確認。

### 2026-09-29: 現行設定と変更候補

- `crates/llm-gateway/src/preset/pricing.rs` の Sonnet 5 行は input $3、output $15、cache write $3.75/$6、read $0.30 と書かれており、現行公式単価と不一致。`claude-sonnet-5-*` が Sonnet 5.5 にも一致するため、このままでは 5.5 を高く見積もる。Sonnet 5 を公式現行単価に直し、5.5 に明示価格行を用意するか、同額と確認した範囲に限り共通化する。価格照合テストと gap detection も更新する。
- `scripts/keepalive-horizon-sim.py` は Sonnet 5 だけ `($0.20, $4)` を持つ。5.5 も同単価で扱うようモデル対応とレポート文言を更新する。`scripts/cache-cost-sim.py` と docs の過去の観測記録は歴史的な測定結果として扱い、現行価格を計算に使う場所のみ再点検する。
- `~/.config/llm-gateway/config-11301-unstable-new.toml` と `config-11302-stable.toml` の personal / bare / emrd の `[ns.*.filter]` は `claude-sonnet-4*` のみ exclude し、5.5 は排除しない。`[ns.*.aliases]` の `claude-sonnet = "claude-sonnet-*"` は探索されたモデル集合次第で 5.5 を指し得るが、provider 別発見・解決を実機で確認してから固定 alias の要否を判断する。既存の `claude-sonnet-5` 明示要求を切り替えるかも別の判断。
- 両 gateway config の 3 namespace にある `[[ns.*.cache]]` は全モデルに `keepalive_horizon = 0.3`。コメントの「5.1 系 22h / 他 5.5h」は cache read $0.20、1h write $4 の Sonnet 5.5 では 0.3 × ($4 / $0.20 × 55 分) = 5.5h となり整合する。Opus 5.5 は $8 / $0.20 で 11h、Fable 5.1 は $20 / $0.25 で 22h なのでコメントを実態に合わせて改める候補。比率自体の変更は実トラフィックでシミュレーション後に判断する。
- `~/.claude-personal/settings.json`、`~/.claude-emrd/settings.json`、`~/.claude-bare/settings.json` の `ANTHROPIC_DEFAULT_SONNET_MODEL` は `claude-sonnet-5[1m]`。5.5 へ切り替えるなら `claude-sonnet-5-5[1m]` を候補とし、CLI バージョンと gateway 解決を検証する。
- `~/.local/share/repos/github.com/kawaz/claude-rules-personal/main/agents/worker-sonnet-low.md` と `worker-sonnet-medium.md` は `model: sonnet[1m]`。上の環境変数を変えると明示 ID を書き換えなくてもワーカーの実モデルが変わり得る。`reference/delegation/model-effort-matrix.md` の sonnet の品質・コスト評価は 5.5 で同一と決めつけず、実作業で再評価する。
- リポ内 `crates/llm-gateway/src/discovery.rs` / `src/config.rs` / `src/router.rs` と研究記録の Sonnet 5 言及はテスト fixture や当時の観測を含む。5.5 の発見・routing の回帰テストを追加する際には、過去記録を機械的に置換しない。

## 暫定的な結論

Sonnet 5.5 は Sonnet 5 と同じ公定単価で 1M context を持つ。まず価格表の Sonnet 5 の公式単価との乖離と 5.5 の glob 吸収を直す対象として明示し、その後に gateway 経由のモデル発見・経路・1M context を検証して運用設定の切り替えを判断する。モデル設定ファイル、agent 定義、実装は本調査では変更していない。

## 未確認事項・kawaz 裁定が要る点

- 現ローカル Claude Code は `claude --version` で 2.1.284 と確認済み。接続先ごとの 5.5 提供状況、`sonnet[1m]` の解決先、gateway 経由の実リクエスト・返却値は未確認。価格表の修正と運用モデルの切り替えは分離できる。
- 3 環境を一斉に 5.5 に上げるか、`worker-sonnet-*` だけ先行・据え置きするか、明示 `claude-sonnet-5` のセッションも移行するかは運用判断が必要。
- `keepalive_horizon` の比率 0.3 を再計算するかは、5.5 利用後の実測ログが必要。同じ単価の Sonnet 5 からの移行だけなら理論上の分岐時間は同じだが、タスク時間や cache hit の実態は未測定。
- 性能の自環境での再現、provider 別の受け入れ状況、Sonnet 5.5 への切り替え時に thinking・tool_choice の破壊変更が実通信に影響するかは未確認。

## 関連

- [公式モデルページ](https://platform.claude.com/docs/en/models/sonnet-5-5/overview)
- [公式価格表](https://platform.claude.com/docs/en/about-claude/pricing)
- [モデル ID 規則](https://platform.claude.com/docs/en/about-claude/models/model-ids-and-versions)
- [Sonnet 5.5 の変更点](https://platform.claude.com/docs/en/models/sonnet-5-5/whats-new-sonnet-5-5)
- [Anthropic 発表](https://www.anthropic.com/claude-sonnet-5-5)
- [Claude Code モデル設定](https://code.claude.com/docs/en/model-config)
- [Claude Code changelog](https://code.claude.com/docs/en/changelog)
