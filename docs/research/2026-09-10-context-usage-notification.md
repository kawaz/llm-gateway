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
| window の大きさ | `SessionStart` の `model` 欄 (`claude-opus-5[1m]` と `[1m]` 付きで来る)、以降の変更は `PostModelSwitch` の `to_model` | 実機確定 (A7) |
| 閾値状態 | `session_id` ごとのファイル (`$XDG_STATE_HOME/...`) | 実装して実機確定 (A6) |
| 文面の注入 | hook stdout の `hookSpecificOutput.additionalContext` | 実機確定 (A4) |

**穴は無い。** window は当初 transcript / upstream リクエスト / env のどこにも `[1m]` が
来ず設定値で補う設計だったが、公式ドキュメントで `SessionStart` だけが `model` 欄を
持つと分かり、実機で `[1m]` 付きの値が来ることを確認した (A7)。

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
    "SessionStart":     [{"hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py model"}]}],
    "PostModelSwitch":  [{"hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py model"}]}],
    "Stop":             [{"hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py measure"}]}],
    "PostToolUse":      [{"matcher": "*", "hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py deliver"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py deliver"}]}]
  }
}
```

`model` が window を state に記録し、`measure` / `deliver` は**話すタイミングの違いだけ**
で、**測定は Stop / PostToolUse / UserPromptSubmit の 3 event すべてで走る**
(理由は下の「Stop 時点の transcript は競合する」):

- **`SessionStart` / `PostModelSwitch` が window を決める**。この 2 つだけが model 名を
  `[1m]` 付きで持つ。`PostModelSwitch` はユーザの `/model` だけでなく Claude Code 自身
  による復元でも飛ぶ (公式 docs) ので、セッション途中の切り替えにも追随する
- **`Stop` は測って黙る**。ターンの起点が何であれ必ず発火する (下表) ので、閾値を
  跨いだことを取りこぼさない
- **配るのは次の `PostToolUse` / `UserPromptSubmit`**。跨ぎを state に書いておき、
  次にモデルが動くときに `additionalContext` で出す。**注入のためだけのリクエストを
  1 本増やさない**
- **90 % 以上だけは `Stop` から直接出す**。この場合だけ継続ターンが 1 本増えるが、
  残り 10 % を切ってから次のターンまで黙っているほうが害が大きい。
  `decision: "block"` ではなく `additionalContext` を使う (理由は後述)
- latch の明示リセットは不要。「下がったら黙って latch を戻す」で clear / compact を
  吸収できる (`PreCompact` / `PostCompact` を足してもよいが、無くても動く)

### Stop 時点の transcript は競合する

**`Stop` hook が走る時点で、そのターンの assistant 行がまだ transcript に無いことが
ある。** 1 往復だけの `claude -p` を同じ設定で 4 回走らせたところ、3 回は Stop での
読み取りが空振りし (state ファイルが作られない)、1 回だけ間に合った。hook スクリプトに
`tee` を 1 段挟んだ (= 数 ms 遅れた) 回は成功しており、**タイミング依存の競合**で
あって設定の問題ではない。

したがって **`Stop` だけで測る設計にしない**。3 event すべてで測れば、Stop が
空振りしても次の `UserPromptSubmit` / `PostToolUse` が拾う (そちらはターンの頭なので
transcript が落ち着いている)。measure は state を進めるだけなので、多重に走っても
`band == prev` で黙る。

なお使用量はもともと **1 リクエスト遅れ**の値なので、この競合で 1 ターン遅れても
性質は変わらない。

### 起点別の hook 発火 (実機)

「user prompt 無しでターンが進む経路で hook が発火しないのでは」という懸念を、
起点 7 種で実測した。結果は **`UserPromptSubmit` と `Stop` はどの起点でも必ず発火する**:

| 起点 | UserPromptSubmit | PreToolUse | PostToolUse | PostToolBatch | Stop |
|---|---|---|---|---|---|
| (a) TUI の user prompt (tool あり) | ✓ | ✓ | ✓ | ✓ | ✓ |
| (e) TUI の user prompt (tool なし) | ✓ | – | – | – | ✓ |
| (b) messaging socket への外部注入 | ✓ | – | – | – | ✓ |
| (c) 別セッションからの SendMessage | ✓ | – | – | – | ✓ |
| (d) background task の完了通知 | ✓ | – | – | – | ✓ |
| (f) `CronCreate` の one-shot 発火 | ✓ | – | – | – | ✓ |
| (g) `ScheduleWakeup` の wakeup | ✓ | – | – | – | ✓ |

理由は wire protocol 側にある。socket に届いた `{"type":"user",...}` は
`mode:"prompt"` として**プロンプトキューに入る** (research 2026-09-08 の
`[uds-messaging]` 実装) ので、人が打った prompt と同じ経路を通る。cron / wakeup も
同じ形で、`UserPromptSubmit` の `prompt` 欄には登録した文面がそのまま入っていた:

```
(c) "<cross-session-message from=\"uds:/tmp/cc-socks/24634.sock\" from-name=\"ctxprobe-16\"
     from-mode=\"prompting\">\nReply with exactly ECHO-FOX. ...\n</cross-session-message>"
(d) "...<tool-use-id>...</tool-use-id>\n<output-file>...</output-file>\n<status>completed</status>..."
(f) "Reply with the single word CRONFIRE"     ← CronCreate に渡した prompt そのもの
(g) "Reply with the single word WAKEFIRE"     ← ScheduleWakeup に渡した文面そのもの
```

(f) / (g) は probe セッション自身に `CronCreate` / `ScheduleWakeup` を使わせて起こした
(bare の TUI セッションには両 tool がある)。cron は `recurring=false` の one-shot、
wakeup は約 90 秒後を指定し、どちらも予定時刻に `UserPromptSubmit` → `Stop` の 2 つ
だけが飛んだ。

つまり **`PostToolUse` だけが取りこぼす** (tool を使わないターン)。`UserPromptSubmit`
と `Stop` の 2 つを押さえれば起点による穴は無い。

`Notification` (`notification_type: "idle_prompt"`) はターンの合間に別途飛ぶが、
これは「入力待ちになった」の知らせでターンの起点ではない。

**未検証**: Monitor tool の task-notification。probe セッションの tool 一覧に Monitor が
無く発火させられなかった (`CronCreate` / `ScheduleWakeup` はあった)。ただし (d) の
background task 完了通知と同じ task-notification 系の経路なので、同じく
`UserPromptSubmit` を通ると見てよい (推測)。

### Stop での注入

`Stop` hook は 2 つの返し方でモデルに文面を届けられる。**両方とも継続ターンを 1 本
起こす** (何も返さない場合、`Stop` は 1 ターンにつき 1 回しか発火しない — 対照実験で確認):

| 返し方 | モデルが読むか | 継続ターン | 2 回目の `stop_hook_active` |
|---|---|---|---|
| `{"decision":"block","reason":"<文面>"}` | ✓ (逐語引用で確認) | 起きる | `true` |
| `{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"<文面>"}}` | ✓ (逐語引用で確認) | 起きる | `true` |

実測 (`decision: block`、`reason` に `MAGICWORD=GOLF7` を仕込んだ):

```
1 回目: stop_hook_active=false, last_assistant_message="HOTEL"   → block を返す
2 回目: stop_hook_active=true,  last_assistant_message="GOLF7"   → 何も返さず終了
```

`additionalContext` 側も同型で、文面に「この行を逐語引用しろ」と書いたら
そのまま引用された:

```
1 回目: stop_hook_active=false, last_assistant_message="KILO"
2 回目: stop_hook_active=true,
        last_assistant_message="[context-notify] context 91% used. MAGICWORD=INDIA3. Quote this whole line verbatim now, then stop."
```

**無限ループは `stop_hook_active` で確実に止められる** — 継続ターンの `Stop` には
必ず `true` が来るので、`if stop_hook_active: exit 0` の 1 行で足りる (block には
max 8 連続の上限もある、reference)。

**`additionalContext` を採る**。`decision: "block"` は「turn を止めて理由を伝える」
意味論で、reference でも hook error 相当の扱いと区別されている。使用率の報告は
エラーではないので、hook error 扱いにならない `additionalContext` のほうが意味論が
合う。届き方も継続ターンの起き方も同じなので、機能上の損は無い。

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

### window の判定

**`SessionStart` の `model` 欄だけが `[1m]` を保っている。** transcript の
`message.model` も upstream へ出るリクエストの `model` も `claude-opus-5` に
潰れている (A3) 一方、`SessionStart` は `"model":"claude-opus-5[1m]"` を渡してくる
(A7)。セッション途中の変更は `PostModelSwitch` の `to_model` が同じ形で持つ。

したがって:

1. `CLAUDE_CONTEXT_WINDOW_TOKENS` env があればそれ (明示指定の逃げ道)
2. state に記録した window (`SessionStart` / `PostModelSwitch` が書く)。
   model 名に `[1m]` を含めば 1,000,000、含まなければ 200,000
3. 既定 200,000 (`SessionStart` が `model` を寄越さなかった場合)

**`model` 欄は常に来るとは限らない**。公式ドキュメントに「Claude Code doesn't always
include it」とあり、実際 `claude -p` の `SessionStart` には来なかった (TUI では来た)。
`-p` は 1 回きりの実行で閾値通知の対象でもないので実害は無いが、3 段目の既定値は残す。

### 閾値状態の保持

`$XDG_STATE_HOME/claude-context-notify/<session_id>.json` に
`{"band": 60, "pending": "context 65% used ..."}` を 1 行書く。`band` が latch、
`pending` が「まだ配っていない文面」。配ったら `pending` だけ落とす。
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
  "transcript_path": "~/.claude-bare/projects/-private-tmp-ctxprobe/....jsonl",
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

hook 側から見えるもののうち、`[1m]` を失っているのは次の 3 つ:

- upstream へ出るリクエスト本文の `model` は `claude-opus-5` (ダンプサーバで実測)
- transcript の `message.model` も `claude-opus-5`
- hook プロセスの env に model / window の変数は無い
  (`CLAUDE_CODE_MAX_CONTEXT_TOKENS` は settings 由来でたまたま居るだけ。
  `CLAUDE_EFFORT` / `CLAUDE_PID` / `CLAUDE_CODE_SESSION_ID` 等はある)

**保っているのは `SessionStart` の `model` 欄と `Pre`/`PostModelSwitch` の
`from_model` / `to_model` だけ** (A7 で判明)。

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

latch の振る舞いを 6 ケースで確認 (すべて期待どおり):

| ケース | 期待 | 結果 |
|---|---|---|
| `Stop`、band 60 に到達 | 測るだけで黙る、pending に積む | 黙った、state に band 60 + pending |
| 次の `PostToolUse` | 積んだ文面を出す | 出した、pending が消えた |
| さらに次の `PostToolUse` | 黙る | 黙った |
| `Stop`、band 97 に到達 | その場で出す | 出した、state に band 97 |
| その継続ターンの `Stop` (`stop_hook_active: true`) | 黙る | 黙った |
| 使用率が下がった (window 1M 相当) | 黙って latch を 0 へ戻す | 黙った、state に band 0 |

**推奨構成そのままの 2 ターンを TUI で通した** (window 60,000)。1 ターン目の `Stop` が
band 60 を積み、2 ターン目の `UserPromptSubmit` が配り、モデルが逐語引用した:

```
UserPromptSubmit hook additional context: [context-notify] context 60% used (36,242 / 60,000 tokens). 大きな読み込みは要点だけに絞る。
```

90 % 以上の即時経路も実セッションで確認した。`Stop` の `additionalContext` が
transcript に `hook_additional_context` attachment として載り、継続ターンでモデルが
反応した:

```
TEXT: OSCAR
ATTACH hook_additional_context ['[context-notify] context 97% used (33,025 / 34,000 tokens). 直ちに引き継ぎを確定して /compact か /clear へ。']
TEXT: コンテキストが97%使用されています。`/compact` または `/clear` を実行してください。
```

所要時間は 1 回 ~76 ms (5 回 0.38 s、大半が python 起動)。`PostToolUse` ごとに
走らせても体感には出ない。

### 2026-09-10 (A7): 公式一次情報で hook 一覧を取り直した

ローカルの `claude-plugin-reference` skill は更新が滞っていて、**event を 10 個以上
取りこぼしていた**。一次情報で取り直した:

- `https://code.claude.com/docs/en/hooks` (2026-09-10 取得。
  `docs.claude.com/en/docs/claude-code/hooks` は 301 でここへ転送される)
- `https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md`
  (2026-09-10 取得。先頭は v2.1.267 = 実機と同版)

ローカル reference に無く、公式一覧にある event: `PreModelSwitch` / `PostModelSwitch` /
`MessageDisplay` / `DirectoryAdded` / `StopFailure` / `PermissionDenied` /
`Elicitation` / `ElicitationResult` / `UserPromptExpansion` / `TaskCreated` /
`TaskCompleted` / `TeammateIdle`。

#### 設計に効いた 5 点

1. **`SessionStart` だけが `model` 欄を受け取れる** ("Only `SessionStart` hooks can
   receive a `model` field, and Claude Code doesn't always include it")。実機で
   `"model":"claude-opus-5[1m]"` を確認 — **window の穴がこれで塞がった**
2. **`PreModelSwitch` / `PostModelSwitch` は `from_model` / `to_model` を持つ**
   (v2.1.251 で追加)。実機の payload は `[1m]` 付きで、さらに **`context_tokens` を
   持っていた**:

   ```json
   {"hook_event_name":"PostModelSwitch","from_model":"claude-sonnet-5[1m]",
    "to_model":"claude-opus-5[1m]","requested_model":"opus","source":"command",
    "context_tokens":45173,"prompt_cache_warm":true,"cache_ttl":"5m",
    "estimated_cache_write_usd":0.2823,"pricing":"catalog"}
   ```

   同じ瞬間の transcript から出した値は 45,167 で **差は 6 tokens**。A1 の合算式が
   Claude Code 自身の数え方と一致していることの裏取りになる。ただし
   `context_tokens` が来るのは model 切り替えのときだけなので、常時の測定には使えない
3. **`transcript_path` は「written asynchronously, may lag」と明記されている**。
   実測した Stop 時点の競合 (前述) は仕様どおりの挙動で、回避策を持つ設計が正しい
4. **`last_assistant_message` は Stop / SubagentStop の正式な欄**で、docs は
   「最終 assistant テキストが要るなら transcript を読まずこれを使え」と言っている。
   ただし**トークン数は持たない**ので、使用量の測定は transcript 経由のままになる
5. **`PreCompact` / `PostCompact` の matcher は `manual` / `auto`**。latch の明示
   リセットに使えるが、「下がったら黙って戻す」で足りるので必須ではない

#### CHANGELOG から拾えた数値と注意

- **v2.1.247**: Sonnet 5 の auto-compact 窓が 1M 全体になり、**約 967K tokens で
  compact** するようになった (従来 ~934K)。= **1M では 96.7 % 前後で auto-compact が
  走る**ので、kawaz の 97 % 帯はほぼ auto-compact と同着になる。95 % 帯までが
  「本人が畳む」ための実効的な最終警告
- **v2.1.260**: 1M context の Opus / Fable セッションも「1M の直前で」compact する
- **v2.1.259**: **blocking Stop hook が「block 直後のターンで model の reasoning を
  失わせ、モデルによっては prompt cache を外す」不具合が修正された**。修正済みとはいえ
  `decision: "block"` がターンの継続に副作用を持ちうる経路だったことは、
  `additionalContext` を選ぶ判断を裏づける
- **v2.1.261**: `/context` のトークン計算は、count API が使えないとローカル推定に
  切り替わる (= `/context` の表示自体が常に厳密とは限らない)
- **v2.1.257**: 共通欄に `scratchpad_dir` が入った。state の置き場として
  `$XDG_STATE_HOME` の代わりに使う選択肢があるが、**セッション終了で消える前提**の
  場所なので latch には向かない (セッションと寿命を揃えたいなら可)

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
- `autoCompactWindow` 設定の実効範囲 (既定の閾値は CHANGELOG から拾えた: 1M で ~967K)
- 200k を超えたセッションでの `exceeds_200k_tokens` の値
- Monitor tool の task-notification で `UserPromptSubmit` が発火するか
  (probe セッションに Monitor tool が無く発火させられなかった)
- Stop 時点の transcript 競合が、長い会話 (= 書き込み量が多い) でも同じ頻度で
  起きるか。観測したのは 1 往復の `claude -p` 4 回だけ
- `PreCompact` / `PostCompact` の発火 (latch を明示的に戻す経路として使えるか)
- 巨大な transcript (数十 MB) での末尾 400 KB 読みの妥当性 — 末尾に非 sidechain の
  assistant 行が 1 つも無いケース (subagent を連発した直後) で読み幅が足りるか
- `SessionStart` の `source: "compact"` が実際に発火するか (latch 初期化の裏取り)
- `SessionStart` が `model` を寄越さない条件 (公式は「always ではない」とだけ書く。
  実測では TUI で来て `claude -p` では来なかった)
- 公式一覧にある未確認 event の入出力: `MessageDisplay` / `StopFailure` /
  `DirectoryAdded` / `UserPromptExpansion` / `Elicitation` 系 / `TeammateIdle`

## 付録: 参照実装 (A6 で実際に動かしたもの)

plugin にするなら `bin/ctx-notify.py` として置き、上記 `hooks/hooks.json` から
`${CLAUDE_PLUGIN_ROOT}/bin/ctx-notify.py` で呼ぶ。

```python
#!/usr/bin/env python3
"""Notify the session when its context usage crosses a threshold.

argv[1] is the role:
  model    -- wire to SessionStart / PostModelSwitch. Records the context
              window, which only these events spell with the `[1m]` suffix.
  measure  -- wire to Stop. Reads usage, moves the latch, queues the message.
              At 90%+ it also speaks immediately (Stop can inject).
  deliver  -- wire to PostToolUse / UserPromptSubmit. Speaks whatever measure
              queued, so the report rides a turn that was happening anyway.

stdin: hook JSON. stdout: hookSpecificOutput.additionalContext, or nothing.
"""
import json, os, sys, pathlib

BANDS = [20, 40, 60, 80, 90, 95, 97]
URGENT_FROM = 90  # bands at or above this are worth their own turn
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


def state_path(session_id):
    base = os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state")
    d = pathlib.Path(base) / "claude-context-notify"
    d.mkdir(parents=True, exist_ok=True)
    return d / f"{session_id}.json"


def load(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


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


def window_of(model_name):
    """Context window implied by a model name as SessionStart spells it."""
    return 1_000_000 if "[1m]" in (model_name or "") else DEFAULT_WINDOW


def window_for(st):
    override = os.environ.get("CLAUDE_CONTEXT_WINDOW_TOKENS")
    if override and override.isdigit():
        return int(override)
    # Recorded by the `model` role. The transcript spells the model without its
    # `[1m]` suffix, so this is the only place the real window is known.
    return st.get("window") or DEFAULT_WINDOW


def remember_window(ev):
    """SessionStart / PostModelSwitch: record the window for later events."""
    name = ev.get("to_model") or ev.get("model")
    if not name:
        return
    sp = state_path(ev["session_id"])
    st = load(sp)
    st["window"] = window_of(name)
    sp.write_text(json.dumps(st))


def band_of(pct):
    hit = 0
    for b in BANDS:
        if pct >= b:
            hit = b
    return hit


def speak(event, text):
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": event,
        "additionalContext": f"[context-notify] {text}",
    }}))


def measure(ev):
    """Move the latch to match current usage. Returns the band just crossed.

    The transcript is written asynchronously, so at Stop the newest assistant
    line is sometimes not there yet. Measuring on every event instead of only
    at Stop means a missed read is picked up by the next one.
    """
    tp = ev.get("transcript_path")
    if not tp or not os.path.exists(tp):
        return
    got = last_main_usage(tp)
    if not got:
        return
    used, _model = got
    sp = state_path(ev["session_id"])
    st = load(sp)
    win = window_for(st)
    pct = round(used * 100 / win)
    band = band_of(pct)

    prev = st.get("band", 0)
    if band == prev:
        return

    st["band"] = band
    st.pop("pending", None)
    # Going down means a compact or clear reset the usage. Move the latch back
    # without saying anything.
    if band > prev:
        st["pending"] = MESSAGES[band].format(pct=pct, used=f"{used:,}", win=f"{win:,}")
    sp.write_text(json.dumps(st))
    return band if band > prev else None


def take_pending(session_id):
    sp = state_path(session_id)
    st = load(sp)
    text = st.pop("pending", None)
    if text:
        sp.write_text(json.dumps(st))
    return text


def main():
    role = sys.argv[1] if len(sys.argv) > 1 else "deliver"
    try:
        ev = json.load(sys.stdin)
    except ValueError:
        return
    sid = ev.get("session_id")
    if not sid:
        return

    if role == "model":
        remember_window(ev)
        return

    # A continuation turn this hook caused. Measuring again is fine; speaking
    # again would loop.
    if ev.get("stop_hook_active"):
        measure(ev)
        return

    band = measure(ev)
    # Stop only interrupts for a band worth its own turn; anything milder waits
    # for a turn that was going to happen anyway.
    if role == "measure" and not (band and band >= URGENT_FROM):
        return
    text = take_pending(sid)
    if text:
        speak(ev.get("hook_event_name"), text)


if __name__ == "__main__":
    main()
```

## 関連

- `docs/research/2026-09-08-session-injection-and-json-port.md` — messaging socket への注入経路 (gateway から直接注入する案の下地)
- `docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md` — main / sub の判別、system ブロック構造
- `docs/decisions/DR-0012-request-events.md` — request / response イベントの欄と「状態を持たない」方針
- `docs/decisions/DR-0024-cache-strategy-and-keepalive.md` — origin (`main` / `sub` / `oneshot` / `unknown`) の判定表
- `https://code.claude.com/docs/en/hooks` — hook event の一次情報 (2026-09-10 取得)。
  `docs.claude.com/en/docs/claude-code/hooks` からは 301 で転送される
- `https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md` —
  auto-compact 閾値 / `Pre`・`PostModelSwitch` 追加 / Stop block の不具合修正
  (2026-09-10 取得、先頭 v2.1.267)
- `claude-plugin-reference` skill の `reference/agents.md` — plugin の `settings.json` が持てる key。
  同 skill の `reference/hooks.md` は event を 10 個以上取りこぼしているので、
  hook 一覧は上の一次情報を見る
