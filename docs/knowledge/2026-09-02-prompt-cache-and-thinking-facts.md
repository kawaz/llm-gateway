# Prompt cache と thinking の確定事実 (公式 doc + 実測)

出典: 公式 doc `build-with-claude/prompt-caching`、`build-with-claude/thinking`、
`build-with-claude/extended-thinking` (2026-09-02 読解) と、gateway の tap で捕捉した
Claude Code の実リクエスト (同日)。

## 課金区分 (基本 input 単価に対する倍率)

| 区分 | 倍率 |
|---|---|
| 通常 input (最後のブレークポイントより後ろ) | 1.0 |
| 5 分キャッシュ書き込み | 1.25 |
| 1 時間キャッシュ書き込み | 2.0 |
| キャッシュ読み取り | 0.1 (Fable 5.1 / Mythos 5.1 は 0.025 = $0.25/MTok) |

- プレフィックスは位置ごとに 3 区分に分かれる: 最遠ヒット位置 A まで read、
  A〜最後のブレークポイントが write (1h 区間と 5m 区間)、その後ろが通常 input。
  usage の `cache_read_input_tokens` / `cache_creation_input_tokens` / `input_tokens`
  がそのまま対応する
- ヒットのたびに TTL は無料で更新される (read 課金はある)
- TTL の起点は「リクエスト開始時点」であり応答終了時点ではない
- 1h エントリは 5m エントリより前に置く必要がある。各ブレークポイントは独立
  エントリなので、後ろの 5m が失効しても前の 1h は生きる
- 無効化: tools → system → messages の階層で、変更した層以降が無効。thinking
  パラメータ / `output_config.effort` の変更は messages 以降を必ず無効化し、
  モデルによっては tools / system も
- `max_tokens: 0` のプリウォームは公式化 (出力非課金)。ただし拡張思考有効 /
  stream / 構造化出力 / tool_choice=tool|any では拒否
- JSON キー順が変わるとキャッシュが崩れる (gateway は `preserve_order` で対応済み)

## Claude Code の実際のリクエスト形 (tap 実測)

- TTL 指定なし = 5m のみ。`{"type":"ephemeral"}` で `ttl` は付けない
- 明示ブレークポイント方式 (トップレベルの自動キャッシングは未使用)。
  本体: `system[1]`, `system[2]`, `messages[末尾 user].content[0]` の 3 個。
  権限判定 classifier (sonnet): 4 個 (上限)
- 1 ユーザー指示 = モデルのツールラウンドごとに 1 リクエスト。末尾ブレーク
  ポイントは最新の tool_result (user ロール) に移るので、毎ラウンドの write は
  直前の assistant 応答 + tool_result の 1 ラウンド分だけ
- `[1m]` は model 名では送られない (ログ上 0 件)。gateway に届くのは base 名

## thinking

- `signature` = 生の思考連鎖 (完全版) の暗号化。サーバはこれを復号して推論を
  継続する。どの `display` でも署名は同一で、生の連鎖はどの設定でも返らない
- `display: "omitted"` (Fable 5.1 / 5、Opus 5、Sonnet 5 のデフォルト): 本文空 +
  署名。主目的は TTFT 短縮。`summarized` (4.6 以前のデフォルト、本 gateway の
  ns では DR-0016 で強制): 本文に要約 (人間向け、推論には使われない)
- クライアントは受け取ったブロックを改変せず送り返す。改変は 400
- 保持: Opus 4.5+ / Sonnet 4.6+ / Fable / Mythos は過去全ターンの thinking を
  保持し入力課金。旧世代と Haiku は最後のターンのみ (API が自動削除)
- Fable 5.1 固有: thinking ブロックは「生成した会話の中」でのみ有効。system /
  tools / それ以前のメッセージが変わると署名が無効化され API が拒否 or 削除。
  透過 proxy が本文に触る際の禁則として、キャッシュ無効化より重い

## 実験で確定した事実 (2026-09-03、Fable 5.1、gateway 経由)

- **`ttl:"1h"` を書いたブレークポイントは 1h エントリしか読まない**。同じ位置に
  5m エントリがあっても素通りし、その位置までを 1h 単価 (2.0) で全量書き直す。
  逆に **5m フラグ (ttl 指定なし) は 1h エントリも読む**。合成実験 2026-09-03 05:42
  (E 系列): 5m で書いたプレフィックスに新ターン + 全ブレークポイント 1h → read 0 /
  1h で全量 write。続けて 5m で当てると、今度は 1h エントリにヒット。さらに 1h で
  当てると直前の 1h 境界まではヒットし、その間にあった 5m 区間だけを 1h で書き直す
- したがって **「実リクエストは 5m、マーカーだけ 1h」は成立しない**。マーカーが
  毎回「前回の 1h 境界から先」を 2.0 倍で書き直すので、常時 1h より高くつく
  (実機 2026-09-03 05:38 のマーカー: プレフィックス一致・同一 route・5 分以内で
  read 46K / 1h write 664K)。keepalive は「常時 1h + idle 時の refresh ping」が正
  (DR-0024)
- **5m キャッシュ済みプレフィックスに `ttl:"1h"` を当ててヒットさせても TTL は昇格しない**
  (write 課金 0、`ephemeral_1h_input_tokens` 0、6.5 分後にミス → 1h 全量書き)。
  1h エントリはミス時に 2 倍単価で書いた時だけ生まれ、その後は 6.5 分後もヒットする
- 同一本文の replay (差分ゼロ) では新しいエントリは生まれない。1h エントリを作るには
  差分 > 0 の新しいブレークポイントが要る (= マーカー注入方式、DR-0024)
- **拒否応答 (`stop_reason: refusal`) はキャッシュを残さない**: usage に
  `cache_creation` が出ても次の同一リクエストはヒットしない
- **サブスク OAuth 経路は Claude Code の形をしていない request を 429
  `{"type":"rate_limit_error","message":"Error"}` で弾く**。system[0] の billing
  header + `You are Claude Code…` + `metadata.user_id` + `User-Agent: claude-cli/…` +
  `x-app: cli` + beta ヘッダを揃えると同一本文が 200 (どの要素が必須かは未分解)。
  「小 probe だけ 429」の真因とみられる (docs/issue/2026-09-03-oauth-requires-claude-code-shape.md)
- main / subagent の区別: `metadata.user_id` (JSON 文字列) の `parent_session_id` が
  **無ければ main 系** (main、`claude --fork-session` の fork、main 直下の classifier)、
  **あれば subagent 系** (Agent tool / `/subtask` の fork 型 subagent とその classifier、
  値は親 session_id と同一)。fork は gateway からは独立セッションに見える
- `system[0]` は `x-anthropic-billing-header: cc_version=<ver>; cc_entrypoint=cli;`
  で、Claude Code のバージョンは本文からも読める (User-Agent にも入る)
- 過去 7 日の実績 (docs は scripts/cache-cost-sim.py): 費用は write が 6 割超、
  その 88% が 5 分失効後の再構築、再構築の 66% は失効から 60 分以内の再開

## 未検証 (実験で確定させるべき点)

- 200K 超 (1M context 領域) の input 単価割増の有無
