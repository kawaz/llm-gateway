# codex luna@max はコスパ最強か — 裏取りと llm-gateway での実践

「luna を max effort で使うとコスパ最強」という噂の裏取り、llm-gateway での
実現方法、実測検証。関連: [2026-08-11-model-effort-cache-mapping.md](./2026-08-11-model-effort-cache-mapping.md)

## 1. 噂の実態 (裏取り結果)

**根拠のある話だが、「最強」という言い方には留保が要る。**

DeepSWE ベンチマーク (113 件の長時間コーディングタスク、91 リポジトリ・5 言語、
2026-07-25 更新版リーダーボード) での実測:

| モデル/effort | 解決率 | 平均コスト/試行 |
|---|---|---|
| Sol Max | 73% ± 3% | $8.39 |
| Terra Max | 70% ± 3% | $3.96 |
| **Luna Max** | **67% ± 4%** | **$0.61** |
| GPT-5.5 XHigh (旧世代最上位) | 67% ± 6% | $7.23 |

Luna Max は Sol Max に解決率で 6 pt 差、コストは**約 13.75 分の 1**。旧世代の
最上位モデル (GPT-5.5 XHigh) と同水準の点推定値を、約 1/12 のコストで出している。

**留保点**:
- 信頼区間が重なっており、僅差の順位は統計的に確定的ではない (第三者ベンチマーク、
  OpenAI 公式評価ではない)
- **「Luna Max」という単独モデルは存在しない**。Luna (モデル) と Max (effort) は
  独立した 2 つのダイヤルで、その組み合わせに過ぎない
- **effort を上げるとトークン消費量そのものが跳ね上がる**。ベンチマーク計測で
  中央値モデルの出力トークンが 61M のところ、この構成は 130M を記録。同じタスクで
  medium → max にするとコストが増えるのは「深く考えるから」だけでなく**出力量が
  増えるから**でもある。初回トークンまでの時間も伸びる
- 実践記事の推奨は "**まず XHigh で試し、成功基準を満たさなかった時だけ Max に
  引き上げる**"。Max を既定にする運用は推奨されていない

## 2. どんなタスクに向くか

条件がはっきりしている:

- **向く**: スコープが 1 つの明確なパッケージに収まる、成功基準を事前に定義できる、
  完了の証拠を明示できる。具体例: リポジトリの独立部分をチェックリストでスキャン、
  テスト失敗の原因調査 (再現手順つき)、2 文書の差分抽出、スキーマに基づく
  レコード抽出・分類、ログの特定パターン解析
- **向かない**: スコープが曖昧 (「リポジトリを調査して」レベル)、アーキテクチャ等
  高度な判断が要る、ステップ間の依存が強い逐次作業 (調整コストが節約を上回る)、
  複数エージェントが同一ファイルへ書き込む競合リスクがある場面

## 3. 用語の対応 (重要な訂正)

**`effort` は Anthropic (Claude) 側の語彙**。client (Claude Code) は
`output_config.effort` (`low`/`medium`/`high`/`xhigh`/`max`) で送る。

**OpenAI/Codex 側の呼称は `reasoning.effort`** で、Responses API のパラメータ名。
値の語彙は `low`/`medium`/`high`/`xhigh` の 4 段階で、**`max` に相当する値は無い**。

記事や Codex CLI ネイティブの「luna@max」という表現は Codex CLI 自身の `@max` 記法
(reasoning.effort=xhigh 相当への割り当て) を指す。llm-gateway 経由で Claude Code
から使う場合は Anthropic 語彙の `output_config.effort: "max"` を送ると、gateway が
`reasoning.effort: "xhigh"` (OpenAI 側の最深値) に丸めて転送する
(`preset/openai/request.rs` の `named_effort` 実装)。つまり **gateway 経由での
「luna を最も深く使う」は実質 `output_config.effort: "xhigh"` を送ることと同義**
(`max` を送っても `xhigh` に丸められるので結果は変わらない)。

## 4. llm-gateway での実践方法

**config に effort を静的指定する仕組みは無い** (`crates/llm-gateway/src/config.rs`
を確認済み)。gateway は client が送った `output_config.effort` / `thinking` を
そのまま `reasoning.effort` へ写すだけ (`preset/openai/request.rs`)。つまり
**client (Claude Code 等) 側がリクエストで effort=max を送る必要がある**。

Claude Code から使う場合の 1 リクエストの形 (llm-gateway が Anthropic 方言を
受けて OpenAI Responses API へ変換する):

```json
{
  "model": "gpt-5.6-luna",
  "output_config": {"effort": "max"},
  "messages": [...]
}
```

alias 経由 (`config-common.toml` の `[ns.x.aliases]`) で `luna = "gpt-5.6-luna"`
は既に設定済みなので、モデル名は `luna` と打てば通る。effort は client 側の
機能 (Claude Code の `output_config` 相当、または Codex CLI 自体の `@max` 記法)
に依存し、gateway 側で強制する仕組みは現状ない。

## 5. 実測検証 (llm-gateway 経由、2026-08-11)

同一プロンプト (Manacher アルゴリズムによる O(n) 最長回文部分文字列、Python) を
`gpt-5.6-luna` に effort 違いで投げた結果:

| effort | 所要時間 | output_tokens | reasoning_output_tokens |
|---|---|---|---|
| low | 13 秒 | 544 | 270 |
| xhigh | 52 秒 | 2372 | 2070 |

**4 倍近い時間、reasoning トークンは約 7.7 倍**。記事の指摘 (トークン量が跳ね上がる、
初回応答が遅くなる) を実測で裏付けた。このタスクでは low でも十分な出力が
得られており (単純なアルゴリズム問題なので)、effort を上げる価値があるかは
タスクの複雑さ次第という記事の主張とも整合する。

**結論**: 「luna@max がコスパ最強」は誇張ではないが、**常時使うべき設定ではない**。
llm-gateway 的には「Codex worker への委譲時、タスクの難度に応じて client 側で
effort を指定し分ける」運用がそのまま活きる (worker-fleet skill の
「effort は難易度で選ぶ」原則と同じ)。luna を既定 worker にし、難しい局面だけ
effort を xhigh/max に上げる、が実践的な落とし所。

## 出典

- [Using GPT-5.6 Luna at Max in Codex (Majestic Labs)](https://majesticlabs.dev/blog/202608/using-gpt-5-6-luna-at-max)
- [GPT-5.6 Sol, Terra, Luna: Full Benchmark Analysis (The Agent Report)](https://the-agent-report.com/2026/07/gpt-5-6-sol-terra-luna-benchmarks-pricing-analysis/)
