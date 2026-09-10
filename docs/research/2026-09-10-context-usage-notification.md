# メインセッションの context 使用率を測って、閾値を跨いだら本人に知らせる

- Date: 2026-09-10
- Status: In Progress

## 動機

長く回っているセッションほど、あと何割 context が残っているかを本人 (= 動いて
いる Claude 自身) が把握できていない。使用率が 20 / 40 / 60 / 80 / 90 / 95 / 97 %
を跨いだ時点で、そのセッションへ「今どのくらい使っているか、そろそろ何をすべきか」
を書いた文面を注入したい。

必要なのは 2 つに分かれる:

1. **測る場所** — 使用率と window の大きさを、どこから正確に取れるか
2. **届ける場所** — 稼働中のセッションへ、どの経路で文面を入れるか

Claude Code 側 (hook / statusline / transcript) と llm-gateway 側の両方に候補が
あるので、実機でどちらが何を持っているかを確定する。

## 調査範囲

- 扱う: hook の stdin JSON、statusline の stdin JSON、transcript jsonl の `usage`、
  `/context` の表示、hook からの文面注入、upstream へ出るリクエストのヘッダ、
  gateway の request / response イベントの欄
- 扱わない: 実装。閾値の文面そのものの推敲。compact の内部アルゴリズム
- 実機はすべて bare 環境 (`CLAUDE_CONFIG_DIR=$HOME/.claude-bare`、ns `bare`)。
  課金は haiku / opus の最小プロンプトのみ。**bare の `settings.json` は書き換えて
  いない** — probe 用の hook / statusline / env は `--settings <一時ファイル>` で
  重ねて渡し、`/tmp/ctxprobe/` に置いた。戻す作業は不要 (元ファイルに触っていない)
- Claude Code v2.1.267

## 調査メモ

### 2026-09-10 (A1): hook の stdin に token / context の欄は無い

5 event (`SessionStart` / `UserPromptSubmit` / `PreToolUse` / `PostToolUse` /
`Stop`) の stdin を丸ごと落として全キーを走査した。出てきた欄:

| event | 固有の欄 |
|---|---|
| 共通 | `session_id` / `transcript_path` / `cwd` / `hook_event_name` / `prompt_id` / `permission_mode` |
| SessionStart | `source` |
| UserPromptSubmit | `prompt` |
| PreToolUse | `tool_name` / `tool_input` / `tool_use_id` |
| PostToolUse | 上記 + `tool_response` / `duration_ms` |
| Stop | `stop_hook_active` / `last_assistant_message` / `background_tasks` / `session_crons` |

**token 数・context 使用量・window の大きさを示す欄は 1 つも来ない** (確定)。
hook から使用率を知りたければ `transcript_path` を自分で読むしかない。

### 2026-09-10 (A1 続き): transcript の最後の assistant `usage` が「直前リクエストの prompt 全長」

transcript jsonl の `assistant` 行は `message.usage` を持つ。実測 (haiku、2 往復):

```
1 本目: input_tokens=10,  cache_creation=33018, cache_read=0      → 計 33028
2 本目: input_tokens=8,   cache_creation=177,   cache_read=33018  → 計 33203
```

`input_tokens + cache_creation_input_tokens + cache_read_input_tokens` が、その
リクエストで upstream に渡した prompt の全長になる。つまり **最後の assistant 行の
この和 = 直近リクエスト時点の context 使用量**。

注意点 2 つ:

- **1 リクエスト分だけ遅れる**。最後の assistant 自身の `output_tokens` と、その後に
  積まれた tool 結果・ユーザ発言はまだ入っていない
- **transcript には subagent の行も混ざる**。行に `isSidechain` があり、subagent 由来は
  `true`。メインの使用量を出すなら `isSidechain != true` で絞る必要がある

### 2026-09-10 (A2): statusline の stdin には `context_window` がそのまま入っている

statusline は `--print` では呼ばれない (`-p` 実行では 1 度も発火しなかった)。tmux で
TUI を起動して捕まえた実物 (opus[1m]、1 往復後):

```json
{
  "session_id": "b7faad07-...",
  "transcript_path": "/Users/kawaz/.claude-bare/projects/-private-tmp-ctxprobe/....jsonl",
  "cwd": "/private/tmp/ctxprobe",
  "model": { "id": "claude-opus-5[1m]", "display_name": "Opus 5 (1M context)" },
  "workspace": { "current_dir": "...", "project_dir": "...", "added_dirs": [...] },
  "version": "2.1.267",
  "output_style": { "name": "default" },
  "cost": { "total_cost_usd": 0, "total_duration_ms": 7539, "total_api_duration_ms": 0,
            "total_lines_added": 0, "total_lines_removed": 0 },
  "context_window": {
    "total_input_tokens": 32814,
    "total_output_tokens": 4,
    "context_window_size": 1000000,
    "current_usage": { "input_tokens": 2, "output_tokens": 4,
                       "cache_creation_input_tokens": 32812, "cache_read_input_tokens": 0 },
    "used_percentage": 3,
    "remaining_percentage": 97
  },
  "exceeds_200k_tokens": false,
  "fast_mode": false,
  "thinking": { "enabled": true }
}
```

- **`used_percentage` / `remaining_percentage` / `context_window_size` が完成品として
  来る**。自分で割り算する必要がない (`used_percentage` は整数、単位は %)
- `total_input_tokens` は `current_usage` の `input_tokens + cache_creation + cache_read`
  と一致 (32814 = 2 + 32812 + 0)。A1 の transcript から出した値と同じ定義
- **最初の 1 回は `current_usage` / `used_percentage` / `remaining_percentage` が
  `null`** (リクエストがまだ 1 本も飛んでいない状態)。読む側は null を扱う必要がある
- `/context` の表示 (`32.8k/1m tokens (3%)`) と一致した (同セッションで確認)

### 2026-09-10 (A3): window の大きさは model 名の `[1m]` で決まる

| 起動 | `model.id` | `context_window_size` |
|---|---|---|
| `--model haiku` | `claude-haiku-4-5-20251001` | 200000 |
| `--model 'opus[1m]'` | `claude-opus-5[1m]` | 1000000 |

bare の settings には `CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000` が入っているが、
haiku では 200000 が出た。**この env は `context_window_size` を動かしていない** =
model 由来と見てよい。`exceeds_200k_tokens` は別立ての boolean で、どちらの
ケースでも `false` だった (200k 超のセッションでの挙動は未検証)。

statusline を使うなら A3 の判定は不要 — `context_window_size` が来る。

### 2026-09-10 (A4): hook の `additionalContext` で文面はモデルに届く

`PostToolUse` hook から次を stdout に返した:

```json
{"hookSpecificOutput":{"hookEventName":"PostToolUse",
 "additionalContext":"[CTXPROBE] context usage crossed 80% (MAGICWORD=ZEBRA9)"}}
```

モデルは次のように読んだ (逐語引用させた):

```
<system-reminder>
PostToolUse:Bash hook additional context: [CTXPROBE] context usage crossed 80% (MAGICWORD=ZEBRA9)
</system-reminder>
```

**hook 由来と分かる形で、確実に context に入る** (確定)。`UserPromptSubmit` /
`Stop` / `SubagentStop` も同じ欄を持つ (reference で実機検証済み)。

一方 **statusline は表示専用で、注入経路を持たない**。`systemMessage` は UI 表示
だけでモデルに届かない (reference、v2.1.193 実機検証済み)。

自走 trigger も無い — hook から新しいターンを起こす手段は存在しない (reference)。
つまり注入した文面がモデルの目に入るのは、**次にモデルが動くとき**に限る。閾値を
跨ぐのは必ずリクエストの前後なので、`PostToolUse` に載せれば同じターンの続きで
読まれる。

### 2026-09-10 (A5): compact との関係

- bare は `autoCompactEnabled: false`。`autoCompactWindow` という設定キーが
  binary の文字列に存在する (`--autocompact <auto|tokens>` CLI option と対になる、
  reference では 100k〜1M)
- `/context` は `Compact buffer: 3k tokens (0.3%)` を 1 カテゴリとして表示する
  (opus[1m] の 1M window で 3k)。`used_percentage` にこの buffer が含まれるかは未確認
- **compact 後に使用率がどう戻るかは未検証**。会話が短すぎて `/compact` が
  `Not enough messages to compact.` を返し、実測できなかった。ただし compact は
  prompt を作り直すので、次のリクエストの `total_input_tokens` が小さくなる = 使用率が
  下がるのは構造上ほぼ確実 (推測)

閾値通知を作る側の要件としては、**使用率は下がりうる** という前提が要る。
「一度 80 % を撃ったら二度と撃たない」ではなく、下がったら latch を戻す設計にする。

### 2026-09-10 (B1): gateway は使用量を持っているが、イベントには載せていない

gateway は本文の usage を読む役 (`UsageObserver`、DR-0014 §4) を既に持っていて、
`input_tokens` / `cache_creation_input_tokens` / `cache_read_input_tokens` を
metering / stats で数えている (`crates/llm-gateway/src/preset/anthropic/metering.rs`)。
つまり A1 と同じ式の材料は手元にある。

ただし **DR-0012 は「本文もトークン数も載せない」と明記している**。request /
response イベントに使用量の欄は無い。実測 (ns `bare`、opus[1m] の 1 本):

```
data: {"ts":1789017906873,"session_id":"f6445274-...","ns":"bare",
       "model":"claude-opus-5","credential":"claude-kawazzz","status":200,
       "prefix":"fb3b5114","origin":"oneshot","cache_ttl_secs":300,
       "cache_expires_at":1789018206873,"cache_paused":false}
data: {"type":"response","ts":1789017908458,"request_ts":1789017906873,
       "session_id":"f6445274-...","prefix":"fb3b5114","ns":"bare",
       "model":"claude-opus-5","credential":"claude-kawazzz","origin":"oneshot",
       "status":200,"stop_reason":"end_turn","aborted":false}
```

`session_id` はリクエストヘッダ `X-Claude-Code-Session-Id` 由来で、statusline /
hook の `session_id` と同じ値 (実測で一致)。系列を突き合わせる鍵になる。

### 2026-09-10 (B1 続き): window の判定は `anthropic-beta` ヘッダでしかできない

upstream へ出る実物を捕まえるため、`--settings` で `ANTHROPIC_BASE_URL` を
ローカルのダンプサーバに向けて 1 本ずつ流した。

`--model 'opus[1m]'`:

```
POST /v1/messages?beta=true
User-Agent: claude-cli/2.1.267 (external, sdk-cli)
X-Claude-Code-Session-Id: 57b15632-...
anthropic-beta: claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,
  thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,
  mid-conversation-system-2026-04-07,effort-2025-11-24,fallback-credit-2026-06-01,afk-mode-2026-01-31
model=claude-opus-5
```

`--model haiku`:

```
anthropic-beta: interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,
  context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219
model=claude-haiku-4-5-20251001
```

確定した 2 点:

- **`[1m]` は client 側で落ちる**。gateway に届く `model` は `claude-opus-5` で、
  `[1m]` の有無を model 名から読むことはできない
- **`context-1m-2025-08-07` が `anthropic-beta` に入るのは 1M のときだけ**。gateway が
  window を知る手掛かりはこのヘッダだけ (200k が既定、あれば 1M)

metadata の `user_id` は `{"device_id":...,"account_uuid":"","session_id":...}` の
JSON 文字列で、DR-0024 の origin 判定はここと請求ヘッダを見る。上の 1 本は
`claude -p` なので `origin: "oneshot"` と判定されていた (`User-Agent` の `sdk-cli` と
整合)。**閾値通知の対象は `origin: "main"` だけ**で、この判定は既存のまま使える。

### 2026-09-10 (B2): 閾値跨ぎの検出に gateway 側の状態は要らない

「前回の % → 今回の %」を系列ごとに覚えるだけなので、どこで判定しても同じ。
gateway 側で持つなら `session_id` (または DR-0012 の `prefix`) ごとの直近 % を
1 個持つだけで足りる。ただし **gateway に状態を持たせるのは DR-0012 の
「流すのは起きたことだけ。状態は持たない」と正面からぶつかる**。

### 2026-09-10 (B3): 届け方の 3 案

| 案 | 内容 | 得失 |
|---|---|---|
| (a) イベントに使用量を載せ、ccmsg が判定して注入 | request / response event に `context_used_tokens` / `context_window_tokens` / `context_used_pct` を足す | 判定も文面も受け手側で持てて gateway は無状態のまま。ただし **DR-0012 の「トークン数を載せない」を覆す**ので DR の改訂が要る |
| (b) gateway が閾値跨ぎを検知して `context_threshold` イベントを出す | 系列ごとの直近 % を gateway が持ち、跨いだ 1 回だけ出す | 受け手が薄くなる代わりに **gateway が状態を持つ** (DR-0012 と衝突)。閾値の並びも gateway の設定になり、文面の都合で gateway を触ることになる |
| (c) gateway が messaging socket へ直接注入 | 2026-09-08 research の経路をそのまま使う | 経路は実証済みだが **`session_id` → pid → socket path の解決を gateway が背負う**。gateway が Claude Code の内部配置を知る依存が増える |

いずれも gateway 側の測定は **1 リクエスト遅れ** という A1 と同じ性質を持つ
(応答の usage は、その応答を作った prompt の長さなので)。

## 暫定的な結論

### 測るのは Claude Code 側の statusline が素直

**statusline の stdin が `used_percentage` / `context_window_size` を完成品で渡して
くる** (A2)。gateway 側で組み立てると、window は `anthropic-beta` ヘッダから推定
(B1)、使用量は usage から合算 (B1)、main の絞り込みは origin 判定 (DR-0024)、と
3 つの推定を重ねることになる。同じ数字が 1 つの欄で手に入る側を使うほうが素直。

statusline は毎ターン呼ばれるので、そこで % を読んで状態ファイル
(`$XDG_STATE_HOME` あたり) に書き、跨ぎを検知したら「未配達の文面」として残す。

### 届けるのは hook の `additionalContext`

statusline 自体は注入できない (A4)。跨ぎを見つけた statusline が印を置き、
**`PostToolUse` (と `UserPromptSubmit`) hook が印を拾って `additionalContext` で
出す**、の 2 段が一番短い。`PostToolUse` を選ぶのは、閾値を跨ぐのがリクエストの
直後で、同じターンの続きでモデルに読ませられるため。

7 段階の文面は **hook 側 (= Claude Code の設定) に置く**のが自然になる。gateway は
文面も閾値も知らない。

### gateway 側は当面「無し」でよいが、捨てるには惜しい点がひとつ

gateway 側の利点は **セッションの外から俯瞰できる**こと (WebUI で全セッションの
残量を並べる、等)。この用途が要るなら (a) — イベントに数値だけ載せて判定は受け手 —
が唯一 DR-0012 の姿勢 (無状態) と両立する。(b) は状態を、(c) は Claude Code への
依存を gateway に持ち込むので、通知だけのために選ぶ理由が弱い。

つまり **「そのセッションに知らせる」は Claude Code 側で閉じ、「全体を眺める」が
欲しくなった時に (a) を検討する**、という切り分けになる。

## 未検証

- compact 後に `used_percentage` がどう戻るか (会話が短く `/compact` が拒否された)
- `used_percentage` に `Compact buffer` が含まれるか
- 自動 compact の既定閾値と `autoCompactWindow` 設定の実効範囲
- 200k を超えたセッションでの `exceeds_200k_tokens` の値
- statusline がどのタイミングで何回呼ばれるか (ターンごと 1 回とは限らない可能性)
- subagent 稼働中に statusline の `context_window` がメインの値を保つか
- gateway 側 (a) 案での 1 リクエスト遅れが、閾値通知として実用上問題になるか

## 関連

- `docs/research/2026-09-08-session-injection-and-json-port.md` — messaging socket への注入経路 ((c) 案の下地)
- `docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md` — main / sub の判別、system ブロック構造
- `docs/decisions/DR-0012-request-events.md` — request / response イベントの欄と「状態を持たない」方針
- `docs/decisions/DR-0024-cache-strategy-and-keepalive.md` — origin (`main` / `sub` / `oneshot` / `unknown`) の判定表
- `claude-plugin-reference` skill の `reference/hooks.md` — hook の stdin / stdout schema
