# btw の fork が親の prompt cache を読むための形 (launch スクリプト側の直し方と、gateway の A に残る役目)

- Date: 2026-11-22
- 対象: Claude Code 2.1.330、llm-gateway unstable (リポの release build)、Claude Haiku 5.5 だけで測った
- 発端: ccmsg の btw の fork (`claude -p --resume <親> --fork-session ...`) が親の会話の分の cache を読めない件 (正本は claude-ccmsg の `docs/findings/2026-11-22-btw-fork-cache-miss-opus-session.md`、gateway 側の手の評価は [2026-11-22-fork-prefix-divergence-gateway-options](./2026-11-22-fork-prefix-divergence-gateway-options.md))。kawaz の依頼は「組み込みの launch スクリプトとカスタムの launch スクリプトで cache が効く形を探し、効かせるためのスクリプトの直し方まで出す」
- 方法: 使い捨ての config dir (素の Claude Code の設定の写し、plugin なし、base URL を unstable の bare の ns に向けたもの) と、tool を 1 つ持つ手書きの stdio MCP server を用意し、リポの外の一時ディレクトリで親のセッションを作って fork した。親は対話 (hyoui) と `-p` の両方で作った。cache の当たりは fork の最初の request の usage (cache_read / cache_creation) で見て、request の本文は gateway のダンプ (DR-0038) で比べた。Claude Code の振る舞いの出どころは、配布 binary の文字列 (minify された JS) を読んで探した。kawaz が使っているセッションには何も送っていない

## 判明した事実

### 1. 外れの元は、親が Claude Code の知らない model の名前で走っていること

- 親の `--model` が Claude Code の知らない名前 (settings の `ANTHROPIC_DEFAULT_*_MODEL` で別名にしたもの。例: `opus[1m]` → request の `model` が `opus`) だと、Claude Code はその名前の能力を引けず、正式な id (`claude-haiku-5-5` 等) で走る時と request の形が変わる
- btw の fork は正式な id で走る (ccmsg DR-0060 §2)。親が別名で、fork が正式な id だと、次の 2 つが食い違う:
  - **ToolSearch の結果の `Tool loaded.`**: 別名の親は tool_result の 1 block だけを送り、正式な id の fork は `Tool loaded.` の text block を足す。出どころは model の能力 `tool_loaded_text` (正式な id では立つ)
  - **直された tool_use の input**: 次の §2
- 親が正式な id で走っていれば、どちらも起きない。正式な id の親を btw と同じ形で fork すると、親の prefix を全部読んだ (42,451 / 42,451)
- 別名で fork すれば能力は揃うが、履歴の thinking が全部落ちて外れる (ccmsg の findings §3 と同じ)。thinking を残す内部の切り替え (`CLAUDE_CODE_RUSTLING_PIXEL=all`) はこの経路では効かなかった

### 2. 直された tool_use の input は、その tool が最初の request の時に引けないと直した形で送られる

- model が配列の欄を JSON の文字列で書いた時、transcript の `input` は Claude Code が直した形、`wireToolInputs` は model が書いたまま。親は書いたままを送る
- fork は、その tool の定義を最初の request の時に引ければ `wireToolInputs` を送り、引けなければ直した形を送る (binary の警告文: 「no loaded tool for ... (an MCP server still connecting or removed ...); sending its input as displayed instead of the copy recorded from the API response」)
- 別名の親の会話は、tool の定義を会話の中に持たない (tools の欄に最初から並ぶ)。MCP の server の接続が遅いと、fork の最初の request の時に引けず、直した形になった (接続を 8 秒遅らせた server で 3 回とも)
- 正式な id の親の会話は、ToolSearch で読み込んだ tool の定義を会話の中 (role:system の message) に持つ。同じ遅い server でも、fork は書いたままを送った (3 回とも)
- `CLAUDE_CODE_MCP_STARTUP_WAIT_MS` を fork に渡すと、最初の request の前に MCP の server の接続を待ち、遅い server でも書いたままを送った
- 書いたままと直した形の違いが cache を切るかは、文字列の綴りで変わる。prompt cache の key は描画後の形で決まるらしく、`"[\"x\", \"y\"]"` (要素の間に空白あり) と配列 `["x","y"]` は同じ key、`"[\"x\",\"y\"]"` (空白なし) と配列は別の key だった。どちらの綴りで来るかは model 次第なので、書いたままを送るのが正しい

### 3. launch スクリプトに 2 つの環境変数を足すと、client 側だけで親の prefix を全部読める

- `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=15000`: 常に付ける (§2)
- `CLAUDE_CODE_MODEL_CAPABILITIES=-tool_loaded_text`: 親が別名で走っていた時だけ付ける (§1)。正式な id の親に付けると、逆に fork だけが `Tool loaded.` を落として外れた (20,589 / 42,451)
- 親が別名かどうかは、親の transcript の最後の `api-request` の行の `params.model` (親が実際に送った model の名前) が、fork に渡す正式な id と違うかで判る
- 2 本のスクリプトの写しにこれを入れて ccmsg の daemon と同じ入力で起こすと、別名の親では元の写しが部分読み・直した写しが全部読み、正式な id の親ではどちらも全部読み (下の「検証の詳細」の表 2)
- 環境変数は process の環境で渡せばよく、`--settings` を合成しなくてよい。fork の起動に `ANTHROPIC_CUSTOM_HEADERS` を process の環境で渡しても、gateway に届いた (A の名乗りもこの形で載せられる)

### 4. request の違いのうち、cache を切らないもの

親の最後の request と fork の request が次の点で違っていても、fork は親の prefix を全部読んだ:

- system の 0 番 (請求ヘッダの `cc_entrypoint` が `cli` と `sdk-cli`)
- 会話の最初の role:system の message の `output_config` (fork の per-turn effort の印)
- top-level の `safeguards`、`max_tokens`、`thinking.display`、`metadata`

### 5. fork どうしでも cache は当たる

fork は自分の形で cache を書く。後の fork が同じ形なら、親の entry でなく前の fork の entry を読む。同じ親を何度も測る時はこれが混ざるので、比べる組ごとに新しい親を作った (下の表 2)。

## 実用的な示唆

- **launch スクリプトの直し方** (2 本とも、print の起動だけ): 親の transcript の最後の request の model を読み、fork に渡す model と違えば `CLAUDE_CODE_MODEL_CAPABILITIES=-tool_loaded_text` を、いつも `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=15000` を、claude の環境に足す。diff は下の「スクリプトの diff 案」
- **弱い所**: 足す 2 つの環境変数も、親の名前を読む transcript の `api-request` の行も、Claude Code の公開されていない内部 (2.1.330 で binary から読んだもの)。版が上がると黙って効かなくなりうる。効かなくなっても外れるだけで、壊れはしない
- **根の側の手**: 親を正式な id で走らせれば (settings の `ANTHROPIC_DEFAULT_OPUS_MODEL` を `opus[1m]` から `claude-opus-5-5[1m]` にする等)、§1 の違いが起きず、§2 も起きにくく (会話に定義が残る)、ccmsg の findings §4 の「effort を変えると会話の分を全部書き直す」も per-turn effort になって起きない。代わりに model の版を gateway の別名に任せられなくなる。Opus では測っていない
- **gateway の A の役目**: client 側で既知の違いは消せるので、A は今は必須でない。A が効くのは、Claude Code の版が上がって client 側の手が効かなくなった時と、まだ知らない種類の違いが出た時の網。名乗りは process の環境の `ANTHROPIC_CUSTOM_HEADERS` で足りる
- 違いを探す時は、同じ親を何度も fork した結果を混ぜない (§5)。組ごとに親を作り、先に元の形、後に直した形の順で測る

## スクリプトの diff 案

組み込みの launch スクリプト (print の起動の直前):

```diff
 [[ -z $prompt ]] && prompt="."
 
+# A print launch reads the session's prompt cache only if the fork sends the
+# history as the session sent it, and claude's --resume rebuilds two things
+# otherwise (Claude Code 2.1.330):
+# - a tool_use input claude repaired goes out as the model wrote it only when
+#   its tool is known at the first request, so wait for the MCP servers
+#   (a session whose tool came from a server still connecting is sent the
+#   repaired input instead)
+# - a session that ran under a model name claude does not recognize (an
+#   alias from the settings, `opus[1m]`) sent ToolSearch results without the
+#   "Tool loaded." text that the fork, under the model's id, adds
+# The name the session ran under is the model of its last request row in its
+# transcript (in the config dir the cwd's .envrc names).
+fork_env=()
+if [[ $print == true ]]; then
+  fork_env+=(CLAUDE_CODE_MCP_STARTUP_WAIT_MS=15000)
+  sent_model=$("${run[@]}" zsh -c 'f=( ${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects/*/$1.jsonl(N) )
+    (( $#f == 1 )) && grep "^{\"type\":\"api-request\"," $f[1] | jq -r "select(.agentId == null) | .params.model // empty" | tail -n 1' _ "$resume" 2>/dev/null) || sent_model=
+  [[ -n $sent_model && ${sent_model%\[1m\]} != ${model%\[1m\]} ]] &&
+    fork_env+=(CLAUDE_CODE_MODEL_CAPABILITIES=-tool_loaded_text)
+fi
+
 # The prompt on claude's standard input, not in its argv (out of ps, clear of
 # the argument length limit, never read as an option). A here-string is a
 # regular file, so claude reads all of it however late it starts.
-[[ $print == true ]] && exec "${run[@]}" claude "${opts[@]}" <<< "$prompt"
+[[ $print == true ]] && exec "${run[@]}" env "${fork_env[@]}" claude "${opts[@]}" <<< "$prompt"
 exec "${run[@]}" hyoui run --detached -- claude "${opts[@]}" <<< "$prompt"
```

カスタムの launch スクリプト (環境変数を掃除する前に決め、print の exec に渡す):

```diff
 PROMPT_LOCAL=$PROMPT
 
+# print の fork (ccmsg の btw) が元のセッションの prompt cache を読めるよう、--resume が組み立て直す履歴を元のセッションが送った形に揃える (Claude Code 2.1.330 で実測)。
+# - claude が直した tool_use の input は、その tool が最初の request の時に読み込まれている時だけ model が書いたままの形で送られる。MCP の server の接続を待つ
+# - claude が知らない model の名前 (settings の別名 opus[1m] 等) で走ったセッションは ToolSearch の結果に "Tool loaded." を付けずに送っていて、正式な id で走る fork は付ける。セッションの最後の request の行の model が fork の model と違えば付けさせない
+print_env=()
+if [[ -n "${PRINT:-}" && -n "${SESSION_ID:-}" ]]; then
+  print_env+=(CLAUDE_CODE_MCP_STARTUP_WAIT_MS=15000)
+  sent_model=$(CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR:-~/.claude-personal} direnv exec . zsh -c 'f=( $CLAUDE_CONFIG_DIR/projects/*/$1.jsonl(N) )
+    (( $#f == 1 )) && grep "^{\"type\":\"api-request\"," $f[1] | jq -r "select(.agentId == null) | .params.model // empty" | tail -n 1' _ "$SESSION_ID" 2>/dev/null) || sent_model=
+  [[ -n $sent_model && ${sent_model%\[1m\]} != ${MODEL%\[1m\]} ]] && print_env+=(CLAUDE_CODE_MODEL_CAPABILITIES=-tool_loaded_text)
+fi
+
 # 余分な環境変数をクリアする
 ...
 if [[ -n ${opts[(r)-p]:-} ]]; then
-  exec direnv exec . "$BIN_CLAUDE" "${opts[@]}" <<< "$PROMPT_LOCAL"
+  exec direnv exec . env "${print_env[@]}" "$BIN_CLAUDE" "${opts[@]}" <<< "$PROMPT_LOCAL"
 fi
```

A が入った後は、同じ配列に `"ANTHROPIC_CUSTOM_HEADERS=<名乗りのヘッダ名>: $resume"` (カスタムの方は `$SESSION_ID`) を足す。

## 検証の詳細

### 条件

- 親は 6 つ。名前は全部 `🤖gw-fork-cache-*`、model は Haiku 5.5、effort は low
  - A: 正式な id、対話 (ToolSearch、Bash の tool_use、turn の途中の入力を含む)
  - B: 別名 `haiku`、対話 (同上)
  - C / D / D2: 別名 `claude-haiku`、`-p` (ToolSearch と MCP の tool の呼び出し)
  - E: 正式な id、`-p` (同上)
- fork は ccmsg の btw と同じ引数 (`-p --resume <親> --fork-session --session-id <新> --no-session-persistence --model claude-haiku-5-5 --effort low --permission-prompts none --output-format stream-json --verbose --include-partial-messages`)。表 2 は launch スクリプトの写しに daemon と同じ stdin の JSON を渡して起こした
- 「親の prefix」は、親の最後の request の cache_read + cache_creation (fork が全部読めた時の cache_read の値)

### 表 1. fork の形ごとの cache (fork の最初の request)

| 親 | fork に足したもの | cache_read / cache_creation | 親の prefix | 結果 |
|---|---|---|---|---|
| A (正式な id) | なし | 42,451 / 737 | 42,451 | 全部 |
| A | `-tool_loaded_text` | 20,589 / 21,640 | 42,451 | 外れ (fork だけが `Tool loaded.` を落とす) |
| A | MCP の待ち | 42,451 / 737 | 42,451 | 全部 |
| B (別名) | なし | 29,137 / 723 | 29,673 | 部分 (ToolSearch の結果まで) |
| B | 別名で fork (`--model haiku`) | 28,424 / 970 | 29,673 | 外れ (履歴の thinking が全部落ちる) |
| B | 別名で fork + `CLAUDE_CODE_RUSTLING_PIXEL=all` | 29,394 / 0 | 29,673 | thinking は落ちたまま (前の行の fork の entry を読んだ) |
| B | `-tool_loaded_text` | 29,673 / 256 | 29,673 | 全部 |
| C (別名) | なし | 17,448 / 231 | 17,550 | 部分 |
| C | `-tool_loaded_text` | 17,550 / 118 | 17,550 | 全部 |

考察: 別名の親 3 つ (対話 1、`-p` 2) で `Tool loaded.` の違いが出て、能力を落とすと全部読んだ。正式な id の親では違いが無く、能力を落とすと外れた。

### 表 2. launch スクリプトの写し (元の形 → 直した形)

| 親 | スクリプト | cache_read / cache_creation | 親の prefix | 結果 |
|---|---|---|---|---|
| D (別名) | 組み込み、元 | 17,451 / 243 | 17,553 | 部分 |
| D | 組み込み、直した | 17,553 / 129 | 17,553 | 全部 |
| D2 (別名) | カスタム、元 | 17,453 / 309 | 17,556 | 部分 |
| D2 | カスタム、直した | 17,556 / 194 | 17,556 | 全部 |
| E (正式な id) | 組み込み、元 | 18,401 / 74 | 18,401 | 全部 |
| E | 組み込み、直した | 18,420 / 54 | 18,401 | 全部 |
| E | カスタム、直した | 18,420 / 121 | 18,401 | 全部 |

考察: カスタムの写しは、cwd とリポの信頼の判定と remote の確認だけを試験のために外して走らせた (一時ディレクトリはリポの外なので)。print の分かれ道とその前の処理は変えていない。D の組み込みの元の後でカスタムの元を測った回は、組み込みの元の fork が書いた entry を読んで値が混ざったので (§5)、新しい親 D2 で測り直した。

### 表 3. 直された tool_use の input (親 B の transcript の写しで、`wireToolInputs` を JSON の文字列の形にしたもの)

| 写しの元 | MCP の server | fork に足したもの | 送った `tags` | 回数 |
|---|---|---|---|---|
| 別名の親 | すぐ接続 | なし / `CLAUDE_CODE_HUMBLE_HAMMOCK=1` / `=true` | 書いたまま (文字列) | 3 |
| 別名の親 | 8 秒遅れ | なし (能力の有無を両方) | 直した形 (配列) | 3 |
| 別名の親 | 8 秒遅れ | `CLAUDE_CODE_HUMBLE_HAMMOCK=1` | 直した形 (配列) | 1 |
| 別名の親 | 8 秒遅れ | `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=20000` | 書いたまま (文字列) | 2 |
| 正式な id で 1 度 resume した会話 | 8 秒遅れ | なし | 書いたまま (文字列) | 3 |
| 同上から `api-request*` の行を除いた写し | 8 秒遅れ | なし | 書いたまま (文字列) | 1 |

考察: 書いたままを送るかは、記録の行でなく、tool の定義を最初の request の時に引けるかで決まった。正式な id の会話は定義を会話の中に持つので待たなくてよい。

### 表 4. input の綴りと cache の key (gateway に直接 2 本送り、2 本目の cache_read を見た。各組は先頭の文を変えて新しい prefix にした)

| 1 本目の `tags` | 2 本目の `tags` | 2 本目の cache_read / cache_creation | 同じ key か |
|---|---|---|---|
| `"[\"x\"]"` | `["x"]` | 29,051 / 0 | 同じ |
| `"[\"x\", \"y\"]"` | `["x","y"]` | 29,054 / 0 | 同じ |
| `"[\"x\",\"y\"]"` | `["x","y"]` | 23,872 / 5,182 | 別 |
| `"[\"x\",\"y\"]"` | `"x"` (対照) | 23,872 / 5,176 | 別 |

### 確かめていないこと

- Opus / Sonnet / Fable、1M context の会話 (測ったのは Haiku 5.5 だけ)
- 発端のセッションで、permit の tool の MCP の server が fork の最初の request の時に接続中だったこと (仕組みは手書きの遅い server で再現した)
- `CLAUDE_CODE_MCP_STARTUP_WAIT_MS` の値 (15 秒) が足りるか、接続できない server がある時に btw の始まりがどれだけ遅れるか
- Seatbelt で包んだ claude (カスタムの launch スクリプトの一部の面) に `env` の変数が届くか
- 親が、Claude Code の知っている別の版の id (gateway が別の model に読み替えるもの) で走っていた時に、§3 の判定 (名前が違えば `Tool loaded.` を落とす) が合うか
- turn の途中の入力の違い (発端の 583 番目) は、今回の親 4 つでは出なかった (別の worker の比較で、親自身が後で送り直していた)
