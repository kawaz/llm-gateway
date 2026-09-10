# メインセッションの context 使用率を測って、閾値を跨いだら本人に知らせる

- Date: 2026-09-10
- Status: In Progress

## 動機

長く回っているセッションほど、あと何割 context が残っているかを本人 (= 動いて
いる Claude 自身) が把握できていない。使用率が 20 / 40 / 60 / 80 / 90 / 95 / 97 %
を跨いだ時点で、そのセッションへ「今どのくらい使っているか、そろそろ何をすべきか」
を書いた文面を注入したい。

**Claude Code の hook だけで完結する形を本命とする** (2026-09-10 の優先順位)。
gateway 側にも経路はあるが、それは kawaz 専用環境でしか動かない = 誰にでも配れない。
plugin として配れる粒度で成立するかを最優先で確定する。

## 調査範囲

- 扱う: hook の stdin JSON、statusline の stdin JSON、transcript jsonl の `usage`、
  hook からの文面注入、閾値状態の保持、window の判定、plugin として配れるか
- 扱わない: 文面そのものの推敲。compact の内部アルゴリズム
- 実機はすべて bare 環境 (`CLAUDE_CONFIG_DIR=$HOME/.claude-bare`、ns `bare`)。
  課金は haiku / opus の最小プロンプトのみ。**bare の `settings.json` は書き換えて
  いない** — probe 用の hook / statusline / env は `--settings <一時ファイル>` で
  重ねて渡し、`/tmp/ctxprobe/` に置いた
- Claude Code v2.1.267

## 結論から: hook だけで成立する

**成立する。** 必要な部品は 3 つとも hook の届く範囲に揃っていた:

| 要る部品 | どこから取るか | 確定度 |
|---|---|---|
| 使用量 (tokens) | `transcript_path` の最後の非 sidechain assistant 行の `usage` を合算 | 実機確定 (A1) |
| 閾値状態 | `session_id` ごとのファイル (`$XDG_STATE_HOME/...`) | 実装して実機確定 (A6) |
| 文面の注入 | hook stdout の `hookSpecificOutput.additionalContext` | 実機確定 (A4) |

唯一の穴が **window の大きさ** で、`[1m]` は transcript にも hook の stdin にも
env にも来ない (A3)。ここだけ設定値で補う (後述)。

statusline は使わない。`used_percentage` を完成品で持っている (A2) 反面、
**plugin の `settings.json` は `agent` と `subagentStatusLine` の 2 key しか
サポートしない** (reference `agents.md`) ため、plugin で配れない。ユーザが既に
自前の statusline を持っている場合に奪うことにもなる。

## 推奨実装 (plugin として配れる粒度)

### hook の配線

`hooks/hooks.json`:

```json
{
  "hooks": {
    "PostToolUse":      [{"matcher": "*", "hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py"}]}],
    "SessionStart":     [{"hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py"}]}]
  }
}
```

- **`PostToolUse` が主役**。閾値を跨ぐのは必ずリクエストの直後なので、ここに載せると
  同じターンの続きでモデルが読む。自走 trigger は存在しない (reference) ので、
  「次にモデルが動くとき」より早くは届かない
- **`UserPromptSubmit` は取りこぼしの受け皿**。tool を 1 つも使わずにターンが終わった
  場合、次のユーザ発言で拾う
- **`SessionStart` は状態の初期化**。`source` が `clear` / `compact` のときに latch を
  戻す。ただし後述の「下がったら黙って戻す」があるので必須ではない
- `Stop` は使わない — `PostToolUse` と二重に撃つだけで、届く早さは変わらない

### 使用量の出し方

transcript jsonl を**末尾から**読み、最初に見つかった条件を満たす行を使う:

- `type == "assistant"` かつ **`isSidechain` が真でない** (subagent の行が混ざる)
- `message.usage` の `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`

これがそのリクエストで upstream に渡した prompt の全長 = **その時点の context 使用量**。
実測 (haiku、2 往復):

```
1 本目: input=10, cache_creation=33018, cache_read=0     → 33028
2 本目: input=8,  cache_creation=177,   cache_read=33018 → 33203
```

ファイル全体を読む必要はない。**末尾 400 KB だけ読んで行に割り、逆順に走査**すれば
足りる (実測: 1 回 ~76 ms、大半が python の起動時間)。

### window の判定 (唯一の穴)

**`[1m]` は client 側で落ちる。** upstream に出るリクエストの `model` は
`claude-opus-5`、transcript に記録される `message.model` も `claude-opus-5` で、
`[1m]` の有無をどちらからも読めない (A3)。hook の env にも model / window の変数は
無い (A3)。

したがって設定値で補う。優先順:

1. `CLAUDE_CONTEXT_WINDOW_TOKENS` env があればそれ (plugin 利用者の明示指定)
2. `ANTHROPIC_DEFAULT_{OPUS,SONNET,FABLE,HAIKU}_MODEL` が
   `<transcript の model>[1m]` の形なら 1,000,000
   (kawaz 環境はこれで自動的に当たる。設定していない人には効かない)
3. 既定 200,000

2 は「その family の既定 alias が 1M なら、このセッションも 1M だろう」という
**推測**で、セッション途中で `/model` を切り替えた場合に外れる。外れても効果は
「閾値が早く / 遅く鳴る」だけなので、1 の明示指定を案内した上で許容する。

### 閾値状態の保持

`$XDG_STATE_HOME/claude-context-notify/<session_id>.json` に
`{"band": 60, "pct": 66, "used": 33134, "window": 50000}` を 1 行書く。
`session_id` は hook stdin の共通欄なので、セッションごとに自然に分かれる。

**上がったときだけ喋る。下がったら黙って latch を戻す。** compact / clear で使用量が
下がるため、「一度 80 % を撃ったら二度と撃たない」にすると compact 後に鳴らなくなる。
逆に下がったことを喋ると、compact の直後に無意味な通知が出る。

跨ぎが 2 段飛んだとき (20 % → 45 %) は **到達した最上位の band だけ**を撃つ。

### 文面

`[context-notify] context 66% used (33,134 / 50,000 tokens). 大きな読み込みは要点だけに絞る。`
の形で `additionalContext` に載せる。モデルには次の形で届く (実機で逐語引用させた):

```
<system-reminder>
PostToolUse:Bash hook additional context: [context-notify] context 66% used (33,134 / 50,000 tokens). 大きな読み込みは要点だけに絞る。
</system-reminder>
```

7 段階の中身は band ごとのテーブルに置く。20 / 40 は数字だけ、60 から行動の指示を
足し、95 / 97 は「新しい調査を始めない」「引き継ぎを確定して畳む」まで書く。
**文面は plugin 側に閉じる** — gateway も ccmsg も関与しない。

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
`transcript_path` を自分で読むしかない。transcript の `usage` から使用量が出せる
ことは上記「使用量の出し方」の通り。

transcript の他の行も当たったが、window の手掛かりは無かった。`attachment` の
`total_tokens_reminder` は `<total_tokens>15000000 tokens left</total_tokens>` で、
これは **プランの残枠**であって context ではない。`~/.claude-bare/session-env/<sid>/`
は空ディレクトリだった。

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

- **`used_percentage` / `remaining_percentage` / `context_window_size` が完成品で来る**。
  hook 方式が抱える window の穴 (A3) がここには無い。`model.id` も `[1m]` 付きで来る
- `total_input_tokens` は `current_usage` の 3 欄の和と一致 (32814 = 2 + 32812 + 0) =
  A1 の transcript から出した値と同じ定義
- **最初の 1 回は `current_usage` / `used_percentage` / `remaining_percentage` が `null`**
- `/context` の表示 (`32.8k/1m tokens (3%)`) と一致した

**それでも採らない理由**: plugin の `settings.json` がサポートするのは `agent` と
`subagentStatusLine` の 2 key だけ (reference `agents.md`) なので、**statusline を
plugin で配れない**。ユーザ自身の statusline を上書きすることにもなる。
自分だけで使うなら A2 のほうが正確 (window が確実)、配るなら hook 方式。

### 2026-09-10 (A3): window は model 名の `[1m]` で決まるが、その `[1m]` が hook 側に来ない

statusline から見た値:

| 起動 | `model.id` | `context_window_size` |
|---|---|---|
| `--model haiku` | `claude-haiku-4-5-20251001` | 200000 |
| `--model 'opus[1m]'` | `claude-opus-5[1m]` | 1000000 |

bare の settings には `CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000` が入っているが
haiku では 200000 が出た。**この env は `context_window_size` を動かしていない**。

一方 hook 側から見えるものはすべて `[1m]` を失っている:

- upstream へ出るリクエスト本文の `model` は `claude-opus-5` (ダンプサーバで実測)
- transcript の `message.model` も `claude-opus-5`
- hook プロセスの env に model / window の変数は無い
  (`CLAUDE_CODE_MAX_CONTEXT_TOKENS` は settings 由来でたまたま居るだけ。
  `CLAUDE_EFFORT` / `CLAUDE_PID` / `CLAUDE_CODE_SESSION_ID` 等はある)

`--model 'opus[1m]'` のときだけ `anthropic-beta` に `context-1m-2025-08-07` が入る
ことは確認したが、これは **upstream へ出る HTTP ヘッダ**なので hook からは見えない
(= gateway 側だけが読める。B 参照)。

### 2026-09-10 (A4): hook の `additionalContext` で文面はモデルに届く

`PostToolUse` hook から `{"hookSpecificOutput":{"hookEventName":"PostToolUse",
"additionalContext":"..."}}` を stdout に返すと、モデルは
`<system-reminder>PostToolUse:Bash hook additional context: ...</system-reminder>`
の形で読む (逐語引用させて確認)。**hook 由来と分かる形で確実に context に入る**。

`UserPromptSubmit` / `Stop` / `SubagentStop` も同じ欄を持つ (reference、実機検証済み)。
`systemMessage` は UI 表示だけでモデルに届かない (reference v2.1.193)。
自走 trigger は存在しない (reference) ので、注入した文面が読まれるのは次にモデルが
動くときに限る。

### 2026-09-10 (A5): compact との関係

- bare は `autoCompactEnabled: false`。`autoCompactWindow` という設定キーが binary の
  文字列に存在し、`--autocompact <auto|tokens>` CLI option と対になる
  (reference では 100k〜1M)
- `/context` は `Compact buffer: 3k tokens (0.3%)` を 1 カテゴリとして表示する
  (opus[1m] の 1M window で 3k)。`used_percentage` にこの buffer が含まれるかは未確認
- **compact 後に使用率がどう戻るかは未検証**。会話が短すぎて `/compact` が
  `Not enough messages to compact.` を返し、実測できなかった。ただし compact は
  prompt を作り直すので、次のリクエストの使用量が小さくなる = 使用率が下がるのは
  構造上ほぼ確実 (推測)

実装側の要件としては **「使用率は下がりうる」を前提に latch を戻す**。これは実測
せずとも安全側に倒せるので、上の推奨実装に組み込んである。

### 2026-09-10 (A6): 参照実装を書いて端から端まで動かした

`ctx-notify.py` (上記の設計そのまま) を `PostToolUse` / `UserPromptSubmit` に配線し、
`CLAUDE_CONTEXT_WINDOW_TOKENS=50000` を渡して 33k 使用のセッションで走らせた。
モデルが読んだ内容 (逐語引用):

```
PostToolUse:Bash hook additional context: [context-notify] context 66% used (33,134 / 50,000 tokens). 大きな読み込みは要点だけに絞る。
```

状態ファイル: `{"band": 60, "pct": 66, "used": 33134, "window": 50000}`

latch の振る舞いを 5 ケースで確認 (すべて期待どおり):

| ケース | 期待 | 結果 |
|---|---|---|
| 状態なし → 56 % | band 40 を撃つ | 撃った、state に 40 |
| 同じ band で再実行 | 黙る | 黙った |
| state が 90、実測 56 % (= compact 相当) | 黙って 40 へ戻す | 黙った、state 40 |
| 戻した直後の再実行 | 黙る | 黙った |
| window 1M で 3 % | band 0、黙る | 黙った |

所要時間は 1 回 ~76 ms (5 回 0.38 s、大半が python 起動)。`PostToolUse` ごとに
走らせても体感には出ない。

### 2026-09-10 (B): hook では取れない情報は何か

gateway 側でしか読めないものは 2 つだけだった:

- **`anthropic-beta` ヘッダ** (`context-1m-2025-08-07` の有無)。A3 の穴を唯一
  確実に塞げる情報だが、HTTP ヘッダなので hook からは見えない
- **他セッションの状況**。gateway は全セッションのリクエストを見ているので、
  「今どのセッションがどれだけ使っているか」を横断で並べられる。hook は自分の
  セッションしか知らない

逆に gateway 側の制約:

- **DR-0012 が「本文もトークン数も載せない」と明記**しており、request / response
  イベントに使用量の欄は無い。載せるなら DR の改訂が要る
- gateway に届く `model` は `claude-opus-5` (client が `[1m]` を落とす) なので、
  window の判定は `anthropic-beta` ヘッダ頼み
- 対象を main に絞る判定は DR-0024 の origin 表で既にある (実測した `claude -p` の
  1 本は `origin: "oneshot"` と判定されていた)
- `session_id` は `X-Claude-Code-Session-Id` 由来で、hook / statusline の
  `session_id` と同じ値 (実測で一致)。突き合わせは可能

実測したイベント (ns `bare`、opus[1m] の 1 本):

```
data: {"ts":1789017906873,"session_id":"f6445274-...","ns":"bare",
       "model":"claude-opus-5","credential":"claude-kawazzz","status":200,
       "prefix":"fb3b5114","origin":"oneshot","cache_ttl_secs":300,
       "cache_expires_at":1789018206873,"cache_paused":false}
data: {"type":"response","ts":1789017908458,"request_ts":1789017906873,...,
       "stop_reason":"end_turn","aborted":false}
```

**「そのセッションに知らせる」用途では gateway を使う理由が無い。** 全セッションを
横断で眺めたくなったとき、DR-0012 を改訂してイベントに数値だけ載せる (判定と文面は
受け手が持つ) のが、gateway を無状態に保てる唯一の形になる。

## 未検証

- compact 後に使用率がどう戻るか (会話が短く `/compact` が拒否された)
- `used_percentage` に `Compact buffer` が含まれるか
- 自動 compact の既定閾値と `autoCompactWindow` 設定の実効範囲
- 200k を超えたセッションでの `exceeds_200k_tokens` の値
- セッション途中で `/model` を切り替えたとき、window 推定 (2 段目) がどう外れるか
- 巨大な transcript (数十 MB) での末尾 400 KB 読みの妥当性 — 末尾に非 sidechain の
  assistant 行が 1 つも無いケース (subagent を連発した直後) で読み幅が足りるか
- `SessionStart` の `source: "compact"` が実際に発火するか (latch 初期化の裏取り)

## 付録: 参照実装 (A6 で実際に動かしたもの)

plugin にするなら `bin/ctx-notify.py` として置き、上記 `hooks/hooks.json` から
`${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py` で呼ぶ。

```python
#!/usr/bin/env python3
"""Notify the session when its context usage crosses a threshold."""
import json, os, sys, pathlib

BANDS = [20, 40, 60, 80, 90, 95, 97]
MESSAGES = {
    20: "context {pct}% used ({used} / {win} tokens).",
    40: "context {pct}% used ({used} / {win} tokens).",
    60: "context {pct}% used ({used} / {win} tokens). 大きな読み込みは要点だけに絞る。",
    80: "context {pct}% used ({used} / {win} tokens). 残りが少ない。作業の区切りを意識する。",
    90: "context {pct}% used ({used} / {win} tokens). 引き継ぎメモを書き始める。",
    95: "context {pct}% used ({used} / {win} tokens). 新しい調査を始めない。今の作業を畳む。",
    97: "context {pct}% used ({used} / {win} tokens). 直ちに引き継ぎを確定して /compact か /clear へ。",
}
DEFAULT_WINDOW = 200_000


def state_dir():
    base = os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state")
    d = pathlib.Path(base) / "claude-context-notify"
    d.mkdir(parents=True, exist_ok=True)
    return d


def tail_lines(path, budget=400_000):
    """Read the last `budget` bytes and return whole lines from it."""
    with open(path, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        f.seek(max(0, size - budget))
        chunk = f.read()
    if len(chunk) < size:
        chunk = chunk.split(b"\n", 1)[1] if b"\n" in chunk else b""
    return chunk.splitlines()


def last_main_usage(path):
    """Prompt length of the most recent main-thread request, or None."""
    for raw in reversed(tail_lines(path)):
        if b'"usage"' not in raw:
            continue
        try:
            d = json.loads(raw)
        except ValueError:
            continue
        if d.get("type") != "assistant" or d.get("isSidechain"):
            continue
        u = (d.get("message") or {}).get("usage") or {}
        total = (
            u.get("input_tokens", 0)
            + u.get("cache_creation_input_tokens", 0)
            + u.get("cache_read_input_tokens", 0)
        )
        if total:
            return total, (d.get("message") or {}).get("model")
    return None


def window_for(model):
    override = os.environ.get("CLAUDE_CONTEXT_WINDOW_TOKENS")
    if override and override.isdigit():
        return int(override)
    # `[1m]` never reaches the transcript, so infer it from the alias the user
    # configured for this model family.
    if model:
        for key in ("OPUS", "SONNET", "FABLE", "HAIKU"):
            alias = os.environ.get(f"ANTHROPIC_DEFAULT_{key}_MODEL", "")
            if alias.startswith(model) and "[1m]" in alias:
                return 1_000_000
    return DEFAULT_WINDOW


def band_of(pct):
    hit = 0
    for b in BANDS:
        if pct >= b:
            hit = b
    return hit


def main():
    try:
        ev = json.load(sys.stdin)
    except ValueError:
        return
    sid = ev.get("session_id")
    tp = ev.get("transcript_path")
    if not sid or not tp or not os.path.exists(tp):
        return

    got = last_main_usage(tp)
    if not got:
        return
    used, model = got
    win = window_for(model)
    pct = round(used * 100 / win)
    band = band_of(pct)

    sf = state_dir() / f"{sid}.json"
    try:
        prev = json.loads(sf.read_text()).get("band", 0)
    except (OSError, ValueError):
        prev = 0

    if band != prev:
        sf.write_text(json.dumps({"band": band, "pct": pct, "used": used, "window": win}))

    # Only announce on the way up. Going down means a compact/clear reset the
    # latch, which must stay silent.
    if band <= prev:
        return

    text = MESSAGES[band].format(pct=pct, used=f"{used:,}", win=f"{win:,}")
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": ev.get("hook_event_name"),
            "additionalContext": f"[context-notify] {text}",
        }
    }))


if __name__ == "__main__":
    main()
```

## 関連

- `docs/research/2026-09-08-session-injection-and-json-port.md` — messaging socket への注入経路 (gateway から直接注入する案の下地)
- `docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md` — main / sub の判別、system ブロック構造
- `docs/decisions/DR-0012-request-events.md` — request / response イベントの欄と「状態を持たない」方針
- `docs/decisions/DR-0024-cache-strategy-and-keepalive.md` — origin (`main` / `sub` / `oneshot` / `unknown`) の判定表
- `claude-plugin-reference` skill の `reference/hooks.md` — hook の stdin / stdout schema
- `claude-plugin-reference` skill の `reference/agents.md` — plugin の `settings.json` が持てる key
