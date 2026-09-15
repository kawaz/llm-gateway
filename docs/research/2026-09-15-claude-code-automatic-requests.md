# Claude Code がユーザ入力と無関係に出す request の棚卸し

- Date: 2026-09-15
- Status: Concluded

## 動機

gateway を通る request のうち、**ユーザが何かを打ったから出たもの**と、**cc が自分の判断で出したもの**の区別が付いていない。後者は `/config` の項目や機能フラグで増減するので、どの項目がどれだけの request を生んでいるかが分かれば、cache 戦略 (DR-0027 の keepalive、DR-0014 の origin 別戦略) の当て先と、課金の内訳が見える。

kawaz の問い: 「`/config` の項目のうち、自動で cc が request を投げそうなものはどれか」。

## 調査範囲

- 扱う: 稼働中の gateway (port 11301) を実際に通った request を tap (DR-0017) で観測し、request 本文の形で分類する。`/config` の項目一覧を cc 本体 (2.1.270) から抽出し、観測された形と突き合わせる
- 扱わない: gateway 側の実装変更提案、Responses 形式 (codex) 経路の内訳
- 一次資料はすべて読み取り専用。プロンプト本文の引用は kawaz の許可済み

## 結論から

52 分の窓で gateway を通った 695 本のうち、**ユーザ入力に紐づかない request は 188 本 (27 %)**。その内訳は auto 権限モードの classifier が 153 本で、他の 3 種 (prompt suggestion 23 / count_tokens 10 / session title 2) を合わせても classifier 1 種に遠く及ばない。

classifier の重さは本数ではなく**本文**にある。監視対象セッションの transcript が丸ごと載るので、1 本あたりの cache 読み出しは中央値 12.2 万・平均 22.3 万トークン (最大 44.9 万)。出力は毎回 9 トークンしかない。同じ窓の全セッションのツール呼び出しは 497 件なので、**ツール呼び出し 3 回に 1 本**ほどの割合で出ている (permission ルールで既に許可済みのツールは classifier を通らないので 1:1 にはならない)。

`/config` の項目のうち request を自力で出すのは prompt suggestion / session recap / auto-compact (+ precompute) / goal evaluator の 4 系統で、**観測できたのは prompt suggestion だけ**。残りは「出す条件が窓の中で成立しなかった」ものと、「そもそも request を出さず tool 定義が増えるだけ」のものに分かれる。

## 観測方法

```bash
curl -sN 'http://127.0.0.1:11301/llm-gateway/tap?include=request_body&max_body=16777216' > tap-full.jsonl
curl -sN 'http://127.0.0.1:11301/llm-gateway/tap?include=response_body&max_body=4096'    > tap-resp.jsonl
```

分類スクリプトは `/tmp/cc-inventory/` に置いた (`report.py` が下の表、`findprompt.py` が形の判定、`dump.py` が 1 件精査)。

### 落とし穴 1: `max_body` が 1 MB だと `system` が丸ごと落ちる

cc が送る JSON の並びは `{"model", "messages", …, "system", "tools"}` で、**`system` と `tools` は末尾にある**。会話が育つと `messages` だけで 1 MB を超えるため、`max_body=1048576` では肝心の `system` が 1 バイトも残らない。同じ event を 1 MB 購読と 16 MB 購読で同時に取って比べた:

| `request_body_size` | 1 MB 購読が受けた長さ | 16 MB 購読が受けた長さ |
| --- | --- | --- |
| 480,564 | 480,564 | 480,564 |
| 2,416,529 | 1,048,576 | 2,416,529 |
| 2,418,135 | 1,048,576 | 2,418,135 |

`max_body` に上限は無く (`tap.rs` の `truncate_string` は文字境界で切るだけ)、複数購読があるときは `capture_plan` が**最大値**を採る。大きい購読を 1 本立てれば、小さい購読しか持たない側も捕り逃さない。購読が登録されきる前の数件だけは前の上限で切られた。

### 落とし穴 2: 自分の調査が classifier の本文に載る

auto モードの classifier は**監視対象セッションの transcript を丸ごと**送る。この調査中に cc 本体から抜いたプロンプト文字列を端末に出すと、それが自分のセッションの transcript に入り、次の classifier request の本文に現れる。生 `grep` で「recap のプロンプトが何件流れたか」を数えると、実際には 0 件でも数十件が当たった。

そこで分類を 2 段にした。**(1) `system` ブロックの役割文で確定する** — ここはクライアントが組み立てる固定文なので会話内容に汚染されない。(2) system が通常の会話用だったものに限り、最終 user message の末尾を見る (recap と prompt suggestion は後述の `cacheSafeParams` により system では見分けられないため)。classifier と確定したものは (2) に掛けない。

## 観測された形

観測窓 2026-09-15 02:34–03:26 UTC (52 分)、`personal` / `emrd` の 2 namespace、対話セッション複数 + サブエージェント多数が稼働中。request 695 本。

| 形 | 件数 | model | max_tokens | tools | cache_control | system の役割ブロック | origin |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cc subagent の通常ターン | 361 | opus-5 299 / sonnet-5 56 / haiku-4.5 6 | 64000 (一部 32000) | 15–26 | 3 | `You are a Claude agent, built on Anthropic's Claude Agent SDK.` | sub 352 / oneshot 9 |
| **auto-mode classifier (stage 1)** | **153** | **sonnet-5** | **64** | **0** | **4** | `You are a security monitor for autonomous AI coding agents.` | sub 89 / main 63 / oneshot 1 |
| cc main セッションの通常ターン | 146 | fable-5-1 94 / opus-5 45 / sonnet-5 7 | 64000 | 23 / 30 / 31 | 4 (58 本は 3) | `You are Claude Code, Anthropic's official CLI for Claude.` | main 139 / sub 7 |
| **prompt suggestion** | **23** | fable-5-1 17 / opus-5 6 | 64000 | 30 | 3 | (通常ターンと同一) | main |
| **count_tokens** | **10** | fable-5-1 7 / opus-5 3 | (無し) | 0 | 0 | (無し) | unknown |
| **session title 生成** | **2** | **haiku-4-5** | 32000 | 0 | **0** | `You are naming a coding session so the user can pick it out …` | oneshot |

太字が「ユーザ入力と無関係に出たもの」= 188 本 / 695 本 (27 %)。

| 形 | cache_strategy | stream | req_bytes 中央値 |
| --- | --- | --- | --- |
| cc subagent の通常ターン | passthrough | true | 393,742 |
| auto-mode classifier | passthrough 90 / keepalive 63 | **false** | 296,047 |
| cc main セッションの通常ターン | keepalive 139 / passthrough 7 | true | 2,224,460 |
| prompt suggestion | keepalive | true | 2,468,819 |
| count_tokens | keepalive | **false** | 62,104 |
| session title 生成 | passthrough | true | 4,186 |

### auto-mode classifier (stage 1)

`permissions.defaultMode = "auto"` (`settings.json`) で有効になる、ツール呼び出しの事前審査。何を審査するかは classifier 自身の system prompt に書いてある — 「the action to evaluate is the **agent's most recent action** (the last tool call in the transcript, always the final entry)」。

本文の構造 (実測 1 件):

```
system[0]  70 バイト   x-anthropic-billing-header: …            cache_control 無し
system[1]  124,284 字  "You are a security monitor for …"       cache_control ephemeral
system[2]  242 字      "## Session Context — User identity: …"  cache_control 無し
messages[0] user 58,756 字  "<user_claude_md>…" (CLAUDE.md 全文)  ブロックに cache_control ×1
messages[1] user 25,861 字  "<transcript>…"                       ブロックに cache_control ×2
```

`max_tokens: 64`、`stream` 無し、`tools` 無し、`stop_sequences` あり。最終 user message の末尾は固定で:

> Stage 1 does NOT apply user intent or ALLOW exceptions — stage 2 will handle those.
> Respond with `<severity>N</severity>` ONLY. Grade HARM ONLY — do NOT reduce for user intent. No other text.

**2 段構成だが、観測窓では stage 1 しか流れなかった** (全 153 本が `<severity>` 版、`<block>` を求める stage 2 版は 0 本)。応答の severity 分布は `{3:13, 4:9, 5:9, 8:67, 10:12, 12:2, 15:10}` で、severity 15 でも stage 2 は続かなかった。よって **stage 2 の発火条件は severity の閾値だけではない**。cc 本体には `xml_2stage` / `stage1RequestId` / `stage2Severity` / `Blocked by fast classifier` の語があるので実装自体は存在する。

応答側の usage 実測 (非 stream 応答 122 本の合計):

| | 合計 | 1 本あたり平均 |
| --- | --- | --- |
| input | 10,980 | 90 |
| cache creation | 221,871 | 1,818 |
| **cache read** | **27,150,820** | **222,547** (中央値 121,848 / 最大 449,478) |
| output | 1,098 | 9 |

つまり **cache には良く乗っている** (毎回 2k 未満の書き足しで済んでいる) が、**読み出し量が会話長に比例して伸びる**。同じ transcript をツール呼び出しのたびに読み直す構造なので、長いセッションほど 1 本が重くなる。観測窓では sonnet-5 の非 stream request は全数が classifier で、他の用途は 1 本も無かった。gateway の stats では claude-sonnet-5 が 2026-09-14 に 844 本 / cache read 83.3 M トークン / \$47.63、2026-09-15 (途中) に 561 本 / 92.1 M / \$45.38 を計上している。

### prompt suggestion

cc 本体のプロンプト:

> Stay silent if the next step isn't obvious from what the user said. … Format: 2-12 words, match the user's style. Or nothing.
> Reply with ONLY the suggestion, no quotes or explanation.

**`cacheSafeParams` を使う。** つまり本会話の `system` と `tools` (30 個) をそのまま流用し、最後に短い指示文 (1,396 字) を足した user message を 1 つ付けるだけ。結果として **request の形は通常ターンと見分けが付かない** — `model` も `max_tokens: 64000` も `tools: 30` も同じで、違うのは最終 user message の中身だけ。

これは gateway にとって好都合で、本会話のプレフィックスにそのまま当たるので cache を汚さず、むしろ TTL を延ばす方向に働く。

### count_tokens

`{model, messages, tools}` だけの本文、`system` も `max_tokens` も無く、応答が 22 バイト。`POST /v1/messages/count_tokens` (gateway は `/v1/messages` と同じ経路で捌く、`llm-gateway-server/src/lib.rs`) の request。`metadata.user_id` を載せないので `origin` は `unknown` になる。

本文は「1 個の user message に 1 ファイルの中身がそのまま入っている」形で、cc が読み込もうとしているファイルのトークン数を測っている。tap には path が出ないので、同定はこの形 (本文の構造 + 22 バイト応答) による。

### session title 生成

**唯一 haiku-4-5 を使う形。** `tools` 無し、`messages` 1 本、本文 4,186 バイト、**`cache_control` が 1 つも無い**。`system` は 3 ブロックで、役割ブロックはこう始まる:

> You are naming a coding session so the user can pick it out of a long list of sessions. The title is a name for what the session is about, not a sentence describing the task: a short noun phrase of two to five words, …
> Return JSON with a single "title" field.

user message は `<session>…</session>` にセッション冒頭の発話を入れただけで、末尾に「Write the title in the predominant language of the session」が付く。

cache breakpoint が 0 個なのは compaction と同じで、**gateway 側の cache 戦略が効かない形**。ただし本文が 4 KB しかないので実害は無い (gateway stats 上も haiku は 2026-09-14 で 72 本 / \$0.46)。観測した 2 本は `cc_entrypoint=sdk-cli` だったため `origin` が `oneshot` になっている。対話セッションから出る分は `main` になるはずだが未確認。

## `/config` の項目ごとの判定

項目名は cc 2.1.270 のバイナリから抽出した `/config` パネルの文字列。

| `/config` 項目 | 自前の request を出すか | 観測 |
| --- | --- | --- |
| Default permission mode (= `auto`) | **出す**。審査対象のツール呼び出しごとに classifier 1 本 | **観測** 153 本 |
| Prompt suggestions | **出す**。`cacheSafeParams` で本会話に相乗り | **観測** 23 本 |
| Auto-compact | **出す**。会話要約 1 本 (`You are a helpful AI assistant tasked with summarizing conversations.`) | 観測されず |
| Precompute compaction | **出す**。上の要約を閾値より手前で先に流す | 観測されず |
| Session recap | **出す**。`cacheSafeParams` で相乗り | 観測されず |
| Claude-proposed goals | **間接的に出す**。goal 自体は `ProposeGoal` tool だが、goal を設定すると毎ターン後に別の evaluator が走る | 観測されず (goal 未設定) |
| Claude-drafted feedback | **出さない**。model が呼ぶ tool が 1 個増えるだけ | 該当なし |
| Ultracode keyword trigger | **出さない**。effort を xhigh に上げるだけ | 該当なし |
| Artifacts / Thinking mode / Fast mode / Output style / 表示系 (Show tips, Reduce motion, Verbose output, Terminal progress bar, Time format, Auto-scroll, Diff tool, …) | **出さない** | 該当なし |
| Messages from your other sessions / External CLAUDE.md includes / Synced project memory | **出さない**。本会話の入力が増えるだけ | 該当なし |

`/config` の項目ではないが自動で出るもの:

| 出所 | 観測 |
| --- | --- |
| `count_tokens` (ファイルのトークン数計測) | **観測** 10 本 |
| session title 生成 (`haikuTitle`) | **観測** 2 本 (haiku-4-5、上記の節) |
| prompt hook の LLM evaluator (`Hook evaluator API error`) | 観測されず (該当 hook 未設定) |

### 観測されなかったものの解釈

**Session recap**: cc 本体のプロンプトは

> The user stepped away and is coming back. Recap in under 40 words, 1-2 plain sentences, no markdown. …

で、5 分以上離席して戻ったときに出る。バイナリにはスキップ理由の文字列が並んでいる — `[awaySummary] skipped: cache age unknown` / `cache stale` / `at or near rate limit` / `draft input present` / `background work pending` / `loop wakeup pending` / `generation in flight`。**観測窓のセッションは常にバックグラウンド作業を抱えていた**ので、`background work pending` で毎回落ちていたと考えるのが自然 (推測、ログ未確認)。`CLAUDE_CODE_ENABLE_AWAY_SUMMARY` という env も存在するが未設定で、それでも同種の `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION` 無しに prompt suggestion は動いたので、env は override であって必須ゲートではない。

**compaction**: 前回調査 (`2026-09-14-claude-code-uncached-requests.md`) で subagent の compaction を同定済み。今回の窓では `compact_boundary` が transcript に 1 件も現れず、request も 0 本だった。**窓が短く、どのセッションも閾値に達しなかっただけ**と見る。

**session title**: gateway stats 上は haiku が日 70 本ほど流れているのに、18 分の窓では 0 本だった。新規セッション開始時にしか出ないので頻度が低い。用途が title 生成であることは未確認 (バイナリに `haikuTitle` / `setHaikuTitle` / `Session Title` の語があるだけ)。

## 未確定として残ったもの

- stage 2 classifier の発火条件。severity 10 でも続かなかったので閾値以外の条件がある
- haiku-4-5 request の中身。observed 0 本のため形を見ていない
- session recap のスキップ理由の裏取り。`--debug` ログを見れば `[awaySummary] skipped: …` が出るはずだが、稼働中セッションの起動オプションは変えられないので未確認
- `origin` が `main` の classifier が 35 本あった。classifier 自身は親セッションの `metadata.user_id` を引き継ぐので、**監視対象が main か sub かがそのまま classifier の origin になる**。gateway 側で「classifier の 1 本」を会話の 1 本と分けて数えたい場合、origin では分けられない

## 関連

- DR-0017 — tap
- DR-0024 — `origin` (main / sub / oneshot / codex / keepalive) の見分け
- DR-0027 — keepalive
- `docs/research/2026-09-14-claude-code-uncached-requests.md` — subagent の compaction request の同定
