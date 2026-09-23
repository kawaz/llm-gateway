# codex 上流は function_call_output に input_image を含む配列を受け付ける

- Date: 2026-09-23

## 判明した事実

- codex 上流 (OpenAI Responses API、codex backend) は `function_call_output.output` に `input_text` と `input_image` を含む配列を渡した request を受け付ける。HTTP 200 で `response.created` (`status: in_progress`) から `response.completed` まで正常に完走し、失敗 event は発生しない
- 画像内容はモデルが実際に利用できる。16×16 の赤一色 PNG (data URL) を渡し「色を 1 語で」と指示したところ、`response.output_text.delta` を連結した結果は `Red` だった。usage は input_tokens 109、output_tokens 5、total_tokens 114、reasoning_tokens 0、cached_tokens 0
- `function_call_output.output` が単純な文字列の従来形式も HTTP 200 で問題なく動作する
- `function_call_output.output` が文字列で、直後の user role メッセージに `input_text` + `input_image` を含む形式 (別経路で画像を渡す形) も HTTP 200 で受け付けられる

## 実用的な示唆 / ベストプラクティス

- `docs/issue/2026-09-19-codex-route-rejects-image-tool-result.md` の方針決定において、画像入り tool 出力を拒否する制約は codex 上流ではなく gateway 側にある。`crates/llm-gateway/src/preset/openai/request.rs` の `tool_result_text()` が text 以外を拒否しているのが原因であり、この変換層の制約を緩めれば画像入り tool 出力を codex ルートで扱える
- 画像を渡す経路は `function_call_output.output` 配列内の `input_image` と、後続 user メッセージでの `input_image` の 2 通りが上流で許容されている。gateway の変換方針を決める際はどちらの経路に寄せるかを別途検討する

## 検証の詳細

### 検証環境

| 項目 | 値 |
|---|---|
| gateway | 稼働中インスタンス `http://127.0.0.1:11301` |
| 入口 | `/ns-personal/v1/responses` (namespace は `ns-` 接頭辞必須、DR-0006) |
| model | `gpt-6-sol` |
| request options | `store: false`, `stream: true` |
| client header | `Content-Type: application/json`, `Accept: text/event-stream`, `originator: codex_exec` |
| 認証 | client 側の `Authorization` は不要 (gateway が上流用認証に差し替える、DR-0025) |
| request 骨格 | `instructions` + `tools: [inspect_image]` + `input: [user メッセージ, function_call (call_id 固定), function_call_output (同 call_id)]` |

### パターン 1: function_call_output.output が文字列 (従来形式)

| 項目 | 結果 |
|---|---|
| HTTP status | 200 |
| 最初の SSE event | `response.created` (`status: in_progress`) |

考察: 従来形式は問題なく動作する。以降のパターンとの対照用ベースライン。

### パターン 2: function_call_output.output が input_text + input_image を含む配列

| 項目 | 結果 |
|---|---|
| HTTP status | 200 |
| 最初の SSE event | `response.created` |
| 終端まで読んだ場合 | `response.created` → `response.completed`、失敗 event なし |
| 画像認識テスト | 16×16 赤一色 PNG を渡し「色を 1 語で」と指示 → `response.output_text.delta` 連結結果は `Red` |
| usage | input_tokens 109 / output_tokens 5 / total_tokens 114 / reasoning_tokens 0 / cached_tokens 0 |

考察: 上流は配列形式の `function_call_output.output` を受け付け、画像内容をモデルが実際に利用できることを確認した。

### パターン 3: output は文字列 + 直後の user role メッセージに input_text + input_image

| 項目 | 結果 |
|---|---|
| HTTP status | 200 |
| 最初の SSE event | `response.created` |

考察: この経路も受け付けられる。ただし完走の確認と画像認識の実証はパターン 2 のみで行っており、このパターンでの `response.completed` までの完走・画像認識は未確認 (推測と分けて記載)。

### 未確認事項

- パターン 3 の完走までの追跡と画像認識結果
- `response.completed` 時点の output item の詳細構造 (worker 側の output_text 抽出結果が空だった事例があるが、delta は正常に取得できているため抽出処理側の問題と推測している。上流仕様の問題ではない)
- 他の codex モデルでの再現性
- 画像サイズの上限
