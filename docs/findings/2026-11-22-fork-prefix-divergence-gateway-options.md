# `--resume --fork-session` の fork が本体の prompt cache を読めない件を、gateway の側でどう扱えるか

- Date: 2026-11-22
- 対象: llm-gateway v0.74.0、Claude Code 2.1.330
- 発端: ccmsg の btw (セッションの横道の質問) は `claude -p --resume <本体> --fork-session --no-session-persistence ...` で本体を fork して走らせる。約 55 万 token の Opus 5.5 の本体で btw を 3 回したら、3 回とも cache_read が system と tools の分 (15,396) だけで、会話の分 (約 53 万) を書き直した。原因の正本は claude-ccmsg の `docs/findings/2026-11-22-btw-fork-cache-miss-opus-session.md`。要点は、fork が transcript から組み立てた messages が本体の送った messages と 5 か所で違ったこと (tool_use の `input` の形が 2 か所、turn の途中で届いた入力の合わせ方、ToolSearch の結果に fork だけ `Tool loaded.` が足されること、SessionStart の system message の effort の印)
- btw の要件 (ccmsg の DR-0060 / DR-0064 / DR-0062 §14): fork は本体と同じ tool の集合と permission mode で走り、書き込みの tool も使う。ccmsg の daemon は fork の標準出力の stream-json を読む。経路・ns・model は本体と同じ。本体が応答中でも btw を許す
- 方法: DR-0024 / DR-0027 / DR-0042 / DR-0038、`docs/findings/2026-11-10-fable-5-2-preserved-thinking-vs-gateway-rewrites.md`、`docs/knowledge/2026-09-02-prompt-cache-and-thinking-facts.md`、控えの置き場のコード (`crates/llm-gateway/src/cache/keepalive/store.rs`) を読んだ。prompt caching の一次資料を読み直した。fork に親を名乗らせる口があるかだけ、upstream に出さない記録用サーバで 1 回測った (「検証の詳細」)。kawaz が使っているセッションには何も送っていない

## 判明した事実

### 1. 外れは「全部か無しか」で、1 か所でも違えば会話の分は丸ごと外れる

- cache の entry は breakpoint の位置にしか書かれず、読む側は自分の breakpoint から 20 位置までしか遡らない (prompt caching「How automatic prefix checking works」: 「Cache writes occur only at your breakpoint」「The lookback window is 20 blocks」)
- Claude Code の fork の breakpoint は system の 2 つと末尾の user の 1 つだけ。本体が書いた entry は本体の各 request の末尾 (= fork の末尾の近く) にしか無い
- したがって、fork の messages のどこか 1 か所が本体と違うと、末尾の近くの entry の hash が全部変わり、読めるのは system の breakpoint まで (今回の 15,396) になる。173 番目の違いを直しても 583 番目が残れば効果はゼロ
- **含意: 違いを種類ごとに 1 つずつ直す案は、知らない種類が 1 つ残るだけで効果が無くなる**。直すなら「本体と同じ並びの範囲は、本体が送った bytes に丸ごと揃える」形でないと効かない

### 2. 本体の送った prefix を、gateway は既に持っている (keepalive の控え)

- keepalive の控え (DR-0027 決定 3) は、系列 (会話の id + `prefix`) ごとに「最後に upstream へ送った 1 本」の本文とヘッダ (認証を除く) を、送出直前の形 (model の書き換え・thinking の変換・cache の整形の後) でファイルに持つ。兄弟の unit (stable / unstable) は同じ置き場を共有する
- 控えがあるのは、main の戦略が `keepalive` の ns の、`tools` を持つ main の会話で、応答が cache に乗った (`hit` / `written` / `partial`) 1 本だけ (DR-0024 §2、DR-0027 決定 8)。oneshot (`claude -p` の fork はこれ) は sub の戦略に落ち、personal の ns では `passthrough` なので控えない
- 控えに入るのは **request だけで、その応答は入らない**。本体が最後の turn に返した答えは控えに無い
- 1 系列の本文は 8MB を蓋にして、超えた系列は控えない (= 延命もしない、`BODY_LIMIT`)。手元の置き場の 17 系列は 224KB〜4.39MB (中央値 1.31MB)。今回の 55 万 token の本体が何 MB かは測っていない
- gateway から見ると fork は**別の会話**: `--fork-session` で会話の id が変わり、origin は `oneshot` (system 先頭の請求ヘッダが `cc_entrypoint=sdk-cli`)。fork の request の中に親の会話の id は無い

### 3. fork に親の会話を名乗らせる口は、Claude Code の既存の設定で作れる

- `--settings '{"env":{"ANTHROPIC_CUSTOM_HEADERS":"<name>: <value>"}}'` で渡したヘッダは、その `claude -p` が出す request の全部に載った (記録用サーバで 8 本中 8 本、改行区切りで 2 つ渡して 2 つとも)。ccmsg の btw は既に `--settings` を渡す起動なので、fork の起動に 1 項目足せば親の会話の id を gateway に伝えられる
- 測ったのは fork なしの `claude -p` 1 回。`--resume --fork-session` でも同じく載るかは測っていない (env はプロセス単位の設定なので変わらない見込み、未検証)

### 4. 本体の bytes に揃えると、thinking の署名は壊れるのではなく元に戻る

- DR-0024 の禁則 (「thinking / system / tools / messages の中身には触らない」) の理由は、過去のメッセージを変えると過去の thinking の署名が無効になること
- 本体の thinking block は本体の送った prefix に対して署名されている。fork が 173 番目を変えて送ると、それより後の本体の thinking は prefix check から見て「前のメッセージが編集された」状態になる (preserved thinking の prefix check:「Changing, inserting, moving, or deleting an earlier message counts, whatever its role」)
- 控えの bytes に揃えるのは、この編集を元に戻す向きの書き換えで、署名との整合は fork の現状より良くなる。ccmsg の findings が「分かっていない」とした「fork の total が本体の直前より約 3 万少ない」は、外れた thinking を API が数えていないと読めば合う (未検証)。揃えれば、fork の model は本体の推論を持ったまま横道に答える
- prefix check を全アカウントで強制する model (Fable 5.2) では、今の fork が 400 になりうる (資料からの推論、未検証)。逆に、揃える範囲の途中で揃えるのを止めると、揃えた部分と揃えなかった部分の境で同じ 400 を誘う。**揃えるなら範囲全体を揃えるか、何もしないかのどちらか**

### 5. tool_choice を変えると messages の cache が外れる

- prompt caching「What invalidates the cache」の表の **Tool choice**: Tools ✓ / System ✓ / Messages ✘、「Changes to the `tool_choice` parameter only affect message blocks」
- 「tool を使わせない」ために `tool_choice` を `none` にすると、会話の分の cache は外れる。tool を使わせない口を作るなら、`tools` も `tool_choice` も本体のまま送り、model が tool_use を出した時の扱いを別に決めることになる

## 責務の判断

gateway の責務は「認証を client の外に置いた薄い proxy」で、本文は素通しが原則。cache については DR-0024 が「どの本文にどの cache 戦略を効かせるか」を gateway の責務として持ち、書き換えてよい範囲を `cache_control` の `ttl` (と `none` の除去) に限っている。DR-0027 で、gateway が自分で 1 本出すこと (同じ本文の送り直し) も責務に入った。

- **B (食い違いを見せる診断の口)**: 責務内。gateway は既に本文を控え (keepalive) と一時ダンプ (DR-0038) で持ち、cache の結果を観測として出している。「同じ prefix のはずの 2 本がどこで違ったか」はその観測の延長
- **A (fork の messages を本体の prefix に揃える)**: cache の費用を下げる点は DR-0024 の目的に入るが、手段が DR-0024 の禁則 (messages に触らない) を越える。やるなら新しい DR で禁則に狭い例外を作る必要がある。例外を正当化できる根拠は §4 (禁則の理由である署名を壊さず、むしろ戻す) と、本体の送った bytes を持っているのが gateway だけ (ccmsg は Claude Code の送る本文を変えられない) という点
- **C (控えに横道の質問を足して gateway が送る口)**: 責務外。「セッションに代わって質問に答える」はアプリケーションの機能で、ccmsg 側の責務。gateway の自送信 (DR-0027) は同じ本文の送り直しで、内容を足さない
- Claude Code の `--resume` が `wireToolInputs` を使わない件そのものは Claude Code の責務。kawaz の判断で上流には報告しない

## 3 案の評価

前提: ccmsg の統括の優先は A の安全性の確認と B が先、C は「tool を使わない btw に限る別の口」として得失だけを出す。

| | A. fork の messages を本体の prefix に揃える | B. 食い違いを見せる診断の口 | C. 控えに質問を足して送る口 |
|---|---|---|---|
| 責務 | 条件付き (DR-0024 の禁則に例外を作る DR が要る) | 責務内 | 責務外 (ccmsg のアプリ機能) |
| 効果 | 今回の形なら会話の分 (約 53 万) が write から read に変わる。5m write (1.25 倍) が Opus 5.5 の read (0.05 倍) になり、1 回の btw の会話の分の費用は約 1/25<br>tool を使う btw にも効く<br>本体の推論 (thinking) も fork に届く | 直接の節約はゼロ<br>原因の特定が、記録用サーバと transcript の blob の手作業の突き合わせから、ダンプ 2 本の比較 1 回になる<br>A の範囲の決め方の検証に要る | tool を使わない btw だけで、prefix は控えと一致するので会話の分を読める |
| 安全性: 署名 | 控えの bytes に揃えるので署名との整合は戻る (§4)<br>途中で止めると 400 を誘うので、範囲全体を揃えるか何もしないかの 2 択にする | 本文に触らない | 控えの bytes そのままなので問題なし |
| 安全性: model が見る内容 | fork の model は「本体の model が実際に見た履歴」を見る。今の fork より本体に忠実<br>境界の 1 件 (控えの末尾の message) に、控えの後に届いた入力が合わさっている場合は、揃えるとその入力が落ちる。境界の扱いは設計で決める | 変わらない | 控えには本体の最後の応答が無い (§2)。足さないと横道の質問が本体の最新の答えを知らずに答える。足すには応答を控えに持たせる欄が要る |
| 安全性: Claude Code の癖への依存 | 違いの種類 (3 種) を知らない形にできる: 「役割の並びと tool_use / tool_result の id の並びが控えと一致する範囲を、控えの bytes に丸ごと置き換える」<br>依存するのは「fork が message の数と順と tool の id を本体と同じに組み立てる」ことだけで、崩れたら素通し (今と同じ) に落ちる | 依存しない | Claude Code の `--resume` を通らないので依存しない<br>ただし控えのヘッダには本体の会話の id が入っているので、gateway が本体の実リクエストと見分けないと控えが btw の本文で上書きされる (keepalive が btw を送り直す) |
| 安全性: 越境 | 親の名乗り (ヘッダ) は client の申告。控えの `ns` が request の ns と同じ時だけ使う<br>tool の id の並びの一致を条件にすると、その履歴を既に持っている client しか揃えられない (id は推測できない) | ダンプと同じ扱い (本文を見せるのは手元の CLI だけ、HTTP の口に本文を出さない DR-0042 の線を保つ) | 新しい転送の口 (ns 認証の下) が要り、ns の token を持つ者が ns 内の任意の会話に質問を足せる |
| 本体の prefix の知り方 | 控え (§2)。fork のヘッダの親の会話 id + ns + 解決後の model で引く | ダンプ 2 本、またはダンプ 1 本と控え | 控え |
| 効かない場面 | 控えが無い会話 (keepalive でない ns、sub、8MB 超、直前の 1 本が cache に乗らなかった)<br>大きい会話ほど 8MB に近づくので、一番効いてほしい会話で効かない恐れ (今回の会話の大きさは未確認) | 無い (ダンプを回せば取れる) | 控えが無い会話 (A と同じ)<br>tool を使う btw 全部 |
| tool を使おうとした時 | 影響なし (fork は普通に tool を使う) | — | `tool_choice: none` にすると messages の cache が外れる (§5)。`tools` と `tool_choice` は本体のままで、tool_use が返ったら「tool は使えない」と返すか、そこで打ち切るしかない |
| 答えの読み方 | ccmsg は今のまま (stream-json) | — | ccmsg の daemon に Messages API の SSE を読む 2 つ目の口が要る (答えの読み方が 2 つになる)。ccmsg 側の DR が要る |
| 控えの鮮度 | 控えは本体の直前に完了した 1 本 (応答を読み切った後に置く)。fork の transcript はそれより 1 往復新しいことがあるが、控えより後ろは fork の形のまま足すので、読めるのは控えの末尾まで | — | 本体の最後の応答が無い (上) |
| ccmsg 側の変更量 | fork の起動の `--settings` に `ANTHROPIC_CUSTOM_HEADERS` を 1 つ足す | 無し (使う時にダンプを回す) | 新しい口の呼び出し、答えの読み方、tool_use の扱い、DR |
| gateway 側の変更量 | 中: 控えの引き当て、範囲の判定、置き換え、ダンプの `request_rewrites` への記録、DR | 小: CLI の比較 1 つ | 中〜大: 新しい転送の口、origin の語、控えに応答を持たせる欄 |

### 他の案

- **D. ccmsg 側で transcript の写しを直してから resume する** (`input` を `wireToolInputs` に差し替えた写しを作り、それを `--resume`): 173 / 297 は直るが 583 / 785 は transcript の行から直せない。§1 のとおり 1 か所でも残れば効果はゼロなので、単独では効かない。gateway の責務外
- **E. 揃えるのを親の名乗り無しで自動にする** (控えの中から、fork の messages と先頭が一致する系列を探す): 名乗らない全ての `--resume` の fork に効くが、request のたびに ns の控え (最大 8MB 毎) を読むことになり、client が頼んでいない本文の書き換えが黙って起きる。採らない

## 推奨

1. **B を先にやる** (CLI の比較)。費用は小さく、責務内で、A の「どの範囲を揃えてよいか」を実物で確かめる道具になる。ccmsg の findings の突き合わせを手作業でなく再現できる形にする
2. **A は B で 3 件以上の実 fork を比べてから、DR を起こして入れる**。明示の名乗り (ヘッダ) を条件にした opt-in にし、範囲は「役割と tool の id の並びが一致する範囲を丸ごと」、揃えきれない時は素通し。DR では DR-0024 の禁則に「親が名乗った fork を、親の控えの bytes に揃える」だけの例外を書く
3. **C は採らない**。tool を使う btw を覆えず、答えの読み方が 2 つになり、控えに応答が無いので鮮度も 1 応答遅れる。tool を使わない btw に限っても、A が入れば同じ cache の効果が今の読み方のまま得られる

起票: `docs/issue/2026-11-22-prefix-divergence-diff.md` (B)、`docs/issue/2026-11-22-align-declared-fork-to-parent-prefix.md` (A、B に依存)

## 実用的な示唆

- `--resume` の fork で cache が外れる原因を探す時は、1 か所見つけて止めない。§1 のとおり違いが 1 つでも残れば全部外れるので、全部の違いを並べてから直し方を決める
- 「同じ本文のはず」の 2 本を比べる時は、`cache_control` と model 名と gateway の書き換え (ダンプの `request_rewrites`) を除いて比べる。受けた形 (ダンプ) と送った形 (控え) を混ぜると、gateway の書き換えが違いとして出る

## 検証の詳細

### fork に任意のヘッダを載せられるか (記録用サーバ、upstream には出していない)

条件:

- 記録用サーバ: 127.0.0.1 の空きポートで受け、request のヘッダ (認証系を除く) と本文の要約 (model、messages の数、metadata) だけを 1 行ずつ保存し、400 を返す
- 起動: リポの外の一時ディレクトリを cwd に、`claude -p --name "🤖gw-btw-header-probe" --no-session-persistence --model claude-haiku-5-5 --settings '{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:<port>","ANTHROPIC_CUSTOM_HEADERS":"x-llm-gateway-probe: parent-abc\nx-second: two"}}' 'hi'`
- Claude Code 2.1.330

結果:

| 記録された request | path | model | `x-llm-gateway-probe` | `x-second` | `x-claude-code-session-id` |
|---|---|---|---|---|---|
| 8 本 (400 を受けての再試行を含む) | `/v1/messages?beta=true` | `claude-haiku-5-5` | 8 本とも `parent-abc` | 8 本とも `two` | 8 本とも同じ値 |

考察: user settings の `env` が base URL を決める環境でも、`--settings` の `env` で base URL と追加ヘッダの両方が上書きされた。追加ヘッダは改行区切りで複数渡せる。`--resume --fork-session` を付けた形は測っていない。

### 控えの大きさ (手元の置き場、ファイルの大きさだけ)

| 系列数 | 最小 | 中央値 | 90 パーセンタイル | 最大 | 2MB 超 |
|---|---|---|---|---|---|
| 17 | 224,553 B | 1,308,493 B | 2,889,109 B | 4,391,505 B | 6 |

考察: 蓋 (8MB) の半分を超える系列が既にある。token 数との対応は控えに usage が無いので取れていない。

### 測らなかったもの

- A の効果 (揃えた fork が会話の分を読むか): 「同じ本文の送り直しは当たる」(DR-0027 の根拠の実測) と「prefix が 100% 一致すれば当たる」(一次資料) から決まるので、実装前に測る価値が小さい。A を実装した時の受け入れ試験で測る
- 使い捨ての Opus のセッションで btw の形を再現すること: user の instruction 群だけで 1 request が 6〜8 万 token になり (ccmsg の findings)、ここで確かめたいこと (gateway の側の手の得失) に対して費用が見合わない

## 分かっていないこと

- `--resume --fork-session` の fork にも `ANTHROPIC_CUSTOM_HEADERS` が載るか (載る見込み)
- 今回の 55 万 token の本体の控えが 8MB に収まっていたか。1M context の会話は蓋を超えうる
- 583 番目 (turn の途中で届いた入力) の違いが、表現だけの違い (同じ文字列の block の割り方) か、内容の違いか。A の境界の扱いを決めるのに要る
- fork の 1 番目の system message の effort の印 (`output_config`) を本体の形に揃えるべきか、fork の形のまま残すべきか。sonnet の 1 件では印の有無が違っても当たった (ccmsg の findings)。A では role:system の message は fork の形のまま残す方が安全と見ている (未検証)
