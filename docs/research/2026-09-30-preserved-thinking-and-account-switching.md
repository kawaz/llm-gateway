# Preserved thinking とアカウント切替（2026-09-30）

## 推奨と判断軸

**推奨（設計候補、未裁定）:** Sonnet 5.5 の thinking を継続利用したい会話では、まず同一アカウントの経路を優先し、通らない場合は既存の fallback を維持したうえで `input_transformations` による reasoning 消失を観測する。全モデル・全会話を一律に hard pin しない。現在の gateway はセッション affinity で直近の成功 route を先頭に戻すが、認証・枠不足時には別アカウントへ自動的に移る。Sonnet 5.5 で移動すると、前のアカウントで生成した thinking block はリクエストが成功してもモデルの入力から消える。なお、現行の affinity は同じ route 名を識別するのであって、異なる route が同じアカウントまたは linked account に属するかを識別しない。

判断軸は (1) 推論の連続性を優先するか可用性を優先するか、(2) 経路の account / linked account 同一性を安全に判定できるか、(3) `input_transformations` を受け取れる upstream と client か、(4) モデル変更やクライアント側の履歴編集による別種の drop / 400 と区別できるか。候補は、同一アカウントへの hard pin（枠不足時に会話停止）、同一アカウント優先＋切替時の loss を観測（可用性維持、推奨）、thinking を除去して切替（reasoning を明示的に捨て、後続の署名連鎖への配慮が必要）、切替を拒否してクライアントに選択させる（既存の自動 fallback から逸脱）。`drop_block` は **prefix mismatch** の 400 回避策であり、別アカウントへの切替を正当化する万能な修復策ではない。

## 一次資料で確定したこと

- [Preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking): “Starting with Claude Fable 5.1, when a `thinking` or `redacted_thinking` block comes back in a request, the API checks the block's `signature` for two things”――生成元モデルの読み取り互換性と、生成時の top-level `system`、`tools`、その block より前の `messages` の prefix 一致。API が返した assistant `content` 配列を、空文字の `thinking` と `signature` も含め順序どおりそのまま次の request に載せる。Fable 5.1 では既定の thinking 本文は空で、推論を運ぶのは署名である。「サーバ側に会話 ID を指定して再開する」仕様ではなく、クライアントが履歴と署名を再送し API が検証する方式である。ただし署名検証の内部保存方式までは公開文書から断定できない。
- 同ページの [account binding](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#account-bound-thinking): “Thinking blocks that Claude Sonnet 5.5 produces work only in the account that produced them, or in an account linked to it. When another account sends one of these blocks, the API drops the block before the model sees it, and the request succeeds.” 旧モデル生成の block は対象外。`linked account` の判定規則、API key が異なるだけで同じ account の場合の挙動、OAuth と API key の同一性、異なる org や Bedrock をまたいだ振る舞いの詳細は記載なし。記録名は `organization_binding_mismatch` だが、文書の規定は **account** であり、org 境界との同一視はしない。
- [model switch](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models): 読めないモデルの block は 400 でなく drop、請求も `input_tokens` カウントもされない。Sonnet 5.5 が作った block を読むモデルは Sonnet 5.5 のみ。Fable 5.1 と Opus 5.5 は prefix 検査対象、Mythos 5.1 と Fable 5.1 より前のモデルは対象外。履歴は削除せず送り続ければ、互換モデルに戻った際は元の block を再び読める。
- [prefix check](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#prefix-check): 先行する本文・tool 結果・system/tool 定義が変わると、その block と後続 block は無効になる。`cache_control` の追加・位置変更・削除、`effort`、`max_tokens`、`output_config`、`tool_choice`、`metadata` は prefix 検査の対象外。thinking block の中間に穴を空けるのは不可。改変された `signature` 自体は常に 400（`drop_block` は効かない）。
- [controls / enforcement](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#preserved-thinking-controls): `anthropic-beta: thinking-binding-controls-2026-08-01` を送ると `input_transformations`（stream では `message_start`）に `thinking_dropped` と `reason: organization_binding_mismatch | model_binding_mismatch | prefix_binding_mismatch` が現れる。Claude API と Google Cloud における account mismatch の報告が明記され、ヘッダがなければ silent drop。`thinking.block_binding.prefix_mismatch_behavior` は `error` / `drop_block` で、ヘッダなしの指定は 400。2026-08-31 00:00 UTC 以後作成の account は prefix 検査が既定で強制（失敗時は 400 または指定による drop）、古い account はフィールドを指定したとき強制。Sonnet 5.5 では `block_binding` を付けられるのは adaptive thinking のみで、`between_tools` 併用は 400。
- [Thinking](https://platform.claude.com/docs/en/build-with-claude/thinking)・[Steering thinking / pricing](https://platform.claude.com/docs/en/build-with-claude/thinking-steering-and-cost#pricing): モデルが推論した token は表示有無によらず output 課金。保持されて入力へ残る過去 turn の thinking は input 課金、drop された block は input 課金されないが推論再生成により後続 token が増える可能性はある。[Extended thinking](https://platform.claude.com/docs/en/build-with-claude/extended-thinking) によれば Sonnet 5.5 / Opus 5.5 / Fable 5.1 の `enabled` + `budget_tokens` は 400 で adaptive を使う。thinking 設定・top-level effort の変更は prompt cache を無効化し得る。`cache_control` marker は署名 prefix に影響せず、両者の条件は同一ではない。

## tool ループ中の置換（2026-09-30、unstable `ns-lab`）

`claude-sonnet-5-5` / adaptive thinking、Claude Code 形の system・metadata・headers、`thinking-binding-controls-2026-08-01` を使用。`ns-lab` は account A の `claude-emrd` に固定。turn 1 で `get_time` の tool_use を要求し、`thinking`、`text`、`tool_use` の順の署名付き content を得た。この content を `/tmp/pt-toolloop-turn1.json` に保存した。次の user message には `tool_result` を入れ、同じ account に以下の 3 通りを送信した。

| ケース | HTTP status | error / transformations と応答状態 |
|---|---|---|
| turn2-a: turn 1 の assistant content をそのまま再送 | 200 | `input_transformations: []`、`stop_reason: max_tokens`、応答は thinking block のみ |
| turn2-b: thinking block を `THINKING:\n` とその本文を持つ text block に置換 | 200 | `input_transformations: []`、`stop_reason: refusal`、応答は thinking block のみ |
| turn2-c: thinking block を削除 | 200 | `input_transformations: []`、`stop_reason: max_tokens`、応答は thinking block のみ |

**観測できた範囲:** 同一 account のこの tool loop では置換・削除とも HTTP 400 にならなかった。ただし turn2-b は `refusal`、対照と削除は `max_tokens` で、正常な回答の継続は確認できていない。`refusal` の要因が履歴改変にあるかどうかも未確認。初回の簡単な tool 指示では thinking なしの `tool_use` のみが返ったため、その試行からは turn 2 を投げず、thinking が得られた試行を対象にした。

account B (`claude-kawazzz`) に `ns-lab` を切り替えた後、保存した同じ turn 1 の履歴と tool_result で再送した。各リクエストの HTTP status は 200、`error` は null。イベントの route 照合は未実施であり、この route 名は切替担当者の申告に基づく。無改変時に `end_user_binding_mismatch` の drop が記録され、切替の実効性とも整合する。

| ケース | HTTP status | error / transformations と応答状態 |
|---|---|---|
| B-unaltered: thinking 無改変 | 200 | `input_transformations: [{"type":"thinking_dropped","path":"messages.1.content.0","reason":"end_user_binding_mismatch"}]`、`stop_reason: max_tokens`、thinking のみ |
| B-replaced: thinking→`THINKING:\n`＋本文の text | 200 | `input_transformations: []`、`stop_reason: refusal`、thinking のみ。`stop_details.category: reasoning_extraction` |
| B-deleted: thinking 削除 | 200 | `input_transformations: []`、`stop_reason: max_tokens`、thinking のみ |

置換の追加試行（いずれも B、`max_tokens: 2048`、各 HTTP 200、`input_transformations: []`、error null）:

| text block の内容 | 試行数 | `stop_reason` | 応答に text があるか |
|---|---:|---|---|
| `THINKING:\n`＋thinking 本文 | 3 | 3 回とも `refusal` (`reasoning_extraction`) | 3 回ともなし |
| thinking 本文だけ | 1 | `max_tokens` | なし |
| `(previous reasoning)\n`＋thinking 本文 | 1 | `refusal` (`reasoning_extraction`) | なし |

`THINKING:` の綴りだけに固有の現象ではなく、reasoning を表す別の見出しでも refusal が再現した。一方、見出しのない本文だけでは `max_tokens` になった。どのケースも完了した回答の text がなく、本文だけの試行も正常完了と同一視できない。各応答は `/tmp/pt-toolloop-B-*-response.json` に保存した。account A の `THINKING:` 置換も `stop_details.category: reasoning_extraction` だった。したがって HTTP 400 にならないことと、継続可能な応答が得られることは区別する。拒否の規則や別の入力への一般化はこの観測だけから確定しない。

### 置換の形（account B）

同じ turn 1 の署名付き履歴を account B に送り、thinking block の置換形だけを変えた。`max_tokens: 4096`、各ケース 1 回（本文だけは 3 回）。`get_time` の tool_result は `2026-09-30T00:00:00Z`。各リクエストは HTTP 200、`input_transformations: []`（無改変のみ `thinking_dropped`）、error null。下表の先頭 60 文字は応答の最初の text block の値。

| ケース | HTTP | `stop_reason` / `stop_details` | `input_transformations` | text block と先頭 60 文字 | `usage.input_tokens` |
|---|---:|---|---|---|---:|
| 本文だけ #1 | 200 | `end_turn` / null | `[]` | あり: `It's **Wednesday, September 30, 2026, 00:00:00 UTC** (ISO 86` | 727 |
| 本文だけ #2 | 200 | `end_turn` / null | `[]` | あり: <code>It's **Wednesday, September 30, 2026, 00:00 UTC** (`2026-09-</code> | 727 |
| 本文だけ #3 | 200 | `end_turn` / null | `[]` | あり: `The current time is **Wednesday, September 30, 2026, 00:00:0` | 727 |
| `🧠 `＋本文 | 200 | `refusal` / `reasoning_extraction` | `[]` | なし | 731 |
| `💬 `＋本文 | 200 | `refusal` / `reasoning_extraction` | `[]` | なし | 730 |
| `(`＋本文＋`)` | 200 | `refusal` / `reasoning_extraction` | `[]` | なし | 729 |
| thinking 削除、tool_result 後の user text に `前の推論:\n`＋本文 | 200 | `refusal` / `reasoning_extraction` | `[]` | なし | 736 |
| thinking 削除 | 200 | `end_turn` / null | `[]` | あり: <code>The current time from `get_time` is:\n\n**Wednesday, September</code> | 646 |
| 無改変 | 200 | `end_turn` / null | `[{"type":"thinking_dropped","path":"messages.1.content.0","reason":"end_user_binding_mismatch"}]` | あり: `The current time is **Wednesday, September 30, 2026, 00:00 U` | 646 |

この条件では本文だけの 3 回は拒否されず回答が完了した。短い接頭辞・括弧・user text への移動はすべて `reasoning_extraction` で拒否された。直前の `max_tokens: 2048` の本文だけ 1 回との差はあり、置換形だけによる効果とは断定できない。応答は `/tmp/pt-replace-forms-*-response.json` に保存した。

## Gateway 照合（文書＋実装）

`docs/DESIGN-ja.md` は現行 tree に存在せず、コード地図は `docs/design/architecture-overview.md` を確認。`docs/decisions/INDEX.md` の DR-0009・0016・0018・0019・0024 を照合した。`crates/llm-gateway/src/session.rs:55-88` は metadata・header・冒頭本文から session key を導出。`crates/llm-gateway/src/router.rs:582-600` は spend_down 昇格後に affinity を先頭へ動かすが、`:605-659` は denial / pace_cap 経路を除外し、後続候補を維持する。affinity は `(namespace, session, model)` で route を記憶する (`router.rs:276`)。DR-0009 は 401/403/429/529 や 5xx を契機に別 credential / upstream へ切替し、2xx に限り affinity を更新する。新しい route が違う account なら Sonnet 5.5 の thinking は消える。成功レスポンスだけでは検知できない。

`crates/llm-gateway/src/preset/anthropic/wire.rs:68-94` は Messages 本文を JSON 再直列化して透過し、thinking block と signature を意図的に削除しない。`:222-274` のテストは `anthropic-beta` ヘッダ保持を確認する。`crates/llm-gateway/src/preset/anthropic/beta.rs:44-95` は Anthropic の beta を基本透過し Bedrock で一部拒否 flag を除く（thinking-binding-controls は固定拒否リスト外）。DR-0016 の opt-in `thinking_display` は top-level `thinking.display` のみ書換え、過去メッセージを改変しない。DR-0024 の cache 戦略は `cache_control` の TTL または marker 除去のみを変更し、thinking・system・tools・messages の本文を変更しない。ただし `crates/llm-gateway/src/preset/openai/request.rs:202-203` の OpenAI 変換経路では thinking block を落とすため、Anthropic 間の切替と同列に連続性を期待できない。

## 検証マトリクス

| ケース | 一次資料からの期待 | 実態 | 根拠 / 制限 |
|---|---|---|---|
| 同じ account / Sonnet 5.5、履歴無改変 | thinking 再利用、200、drop なし | **実測 2026-09-30、gateway 0.60.0、unstable 11301 / `ns-personal`、route `claude-kawazzz`。** beta 有: turn 1 HTTP 200・署名付き thinking・`input_transformations: []`・`thinking_tokens: 110`、turn 2 HTTP 200・`input_transformations: []`・`input_tokens: 504`。beta 無: turn 1 HTTP 200・署名付き thinking・`input_transformations: null`・`thinking_tokens: 109`、turn 2 HTTP 200・`input_transformations: null`・`input_tokens: 494`。 | `/v1/models` は `claude-sonnet-5-5` を返し、SSE の `request` / `response` は各ペアとも同一 route・HTTP 200。両 turn で `response.content` を無改変で再送した。drop 通知がないことは確認したが、モデル内部の thinking 利用そのものは外部から観測できない。 |
| 異なる key だが同一 account または linked account / Sonnet 5.5 | account が同一または linked なら使用可 | **未検証** (手元に同一 account の別 key が無い) | key 単位でなく account 単位との文書の語義。linked の識別規則は未公表。 |
| 異なる account / Sonnet 5.5 (`claude-emrd` で生成 → `claude-kawazzz` に再送) | 200、旧 thinking は drop。beta header 有りなら `organization_binding_mismatch` | **実測 2026-09-30 08:45 JST、gateway 0.60.0、unstable `ns-lab` (routing を 1 本に固定して restart で切替)**。turn2 は beta 有 / 無とも 200。**`input_transformations` は beta 有でも `[]`** (理由の報告は出なかった)。ただし `usage.input_tokens` は 401 (beta) / 409 (無) で、同じ turn1 content を `claude-emrd` 自身に送った対照 517 / 520 より約 110 少なく、turn1 の `thinking_tokens` (114 / 109) にほぼ一致する = **thinking block は黙って落ちている**。両 account は linked ではない (落ちているので) | 一次資料の「header 有りなら報告される」は実機と食い違う (この 2 account の組では `[]`)。drop の検知は `input_tokens` の差でしか見えない。`claude-zunsystem` は sonnet 5.5 の route が無く (404 `no route configured`) 未測 |
| 同一 account / Sonnet 5.5、先行 message を改変 | 新 account は既定で 400、`drop_block` 指定なら 200＋drop | **実測 2026-09-30、gateway 0.60.0、route `claude-kawazzz`。** beta 有で turn 1 の署名付き thinking を保持し、turn 2 の先行 user 本文の数値を 1 箇所変更すると HTTP 200・`input_transformations: [{"type":"thinking_dropped","path":"messages.1.content.0","reason":"prefix_binding_mismatch"}]`・`input_tokens: 392`・`thinking_tokens: 255`。 | `thinking.block_binding.prefix_mismatch_behavior` は指定せずに実施。2026-08-31 00:00 UTC 以降作成 account では既定強制、旧 account は指定時のみ強制との一次資料に照らし、この route は旧 account の挙動と整合するが作成日自体は未確認。 |
| 異なる非 linked account / 旧モデル生成 thinking | account binding による drop は起きない | **API 実機未検証** | “Blocks from earlier models aren't affected.” モデル間互換・署名自体の妥当性は別途必要。 |
| gateway 内 route 切替 | affinity が同 route を優先、denial / pace_cap / 5xx 時に別 route へ移る | **実装と既存 unit test で確認、API 実機未検証** | router.rs:582-659、DR-0009、DR-0018、DR-0019。 |

**未検証と次の測定:** `scripts/preserved-thinking-probe.sh http://127.0.0.1:11301 ns-personal claude-sonnet-5-5 --beta --turn1-out /tmp/pt-content.json` で署名付き content を保存し、同じ引数の `--turn2-in /tmp/pt-content.json` で再送する。`--beta` を外して両ターンを再実行し、先行 user 本文変更は beta 有の turn 2 に `--edit-prefix` を付ける。素の request は OAuth 経路で HTTP 429 `Error` となるため（`docs/issue/2026-09-03-oauth-requires-claude-code-shape.md`）、probe は Claude Code 形の system・metadata・headers を付けて測定した。credential の値は読まない。reload 実装後、unstable のみの `[ns.personal]` の `[[ns.personal.routing]]` に `models = ["claude-sonnet-5-5"]` と `routes = ["<対象 route 名>"]` を既存の `models = ["*"]` より前に置き、承認済みの各 route に切り替えて 2 turn を実行する。route 定義は既存の `[routes.<対象 route 名>]` を使用し、同一 key、別 key 同 account、別非 linked account、linked account を個別に確認する。固定後は SSE `/llm-gateway/events` の `request` / `response` の `credential` と daemon log unstable の `route` で turn ごとの実経路を突き合わせる。実運用 config の編集・reload は別途承認を得てから行う。route 名だけでは account 同一性や linked 判定を推定しない。Google Cloud / Bedrock / OAuth の未測定結果も一般化しない。認証情報探索は権限制御で拒否されたため別経路で回避しない。使用後の署名付き content は外部共有しない。自己検証用ファイルは利用者が適切に管理する。
