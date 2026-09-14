# cache に乗らない保持 request の正体 — Claude Code の会話要約 (compaction)

- Date: 2026-09-14
- Status: Done

## 動機

DR-0027 の replay が保持していた request のうち、`cache_control` が 1 つも無く、
何度送り直しても usage の cache read / creation が 0 のまま (= ログの `cache="none"`)
の系列が 4 本見つかった。これが Claude Code のどの機能の request なのかを、
保持ファイル・transcript・gateway ログの時刻から突き合わせて確定する。

## 調査範囲

- 扱う: 保持ファイル `49a66dfd-….7d121f2e.json` の全パラメータ、system / tools /
  messages の中身、送信時刻と transcript の突合、none 系列 4 本の異同
- 扱わない: 3 本の json が消えた系列の直接確認 (現物が無い)、gateway 側の replay 実装の変更提案
- 一次資料はすべて読み取り専用で参照。プロンプト本文の引用は kawaz の許可済み

## 結論から: subagent の会話を圧縮する compaction request

**`7d121f2e` は Claude Code の会話要約 (compaction) の request である。確度: 確定。**
しかも親セッションの会話ではなく、**subagent (`webui-first-connect`) の会話**を
要約したものだった。

根拠は 3 つが揃っている:

| 観測 | 内容 |
|---|---|
| system 第 3 block | `You are a helpful AI assistant tasked with summarizing conversations.` |
| 最終 user message | 「`<analysis>` と `<summary>` を書け、ツールは一切呼ぶな」という compact 用の指示文 (後述) |
| transcript の突合 | 送信時刻 2026-09-13T14:56:40Z の 87 秒後、subagent の transcript に `subtype: "compact_boundary"` (`Conversation compacted`) が記録されている |

`cache_control` が 0 個なのは **cc が compaction request に cache breakpoint を
張っていないから**で、gateway 側の不具合ではない。

## 観測 1: request の形

`~/.local/state/llm-gateway/stats/keepalive/49a66dfd-8ae0-4388-bdc1-a28c9960308e.7d121f2e.json`
(2.1 MB)。`user-agent: claude-cli/2.1.265 (external, cli)`、route `claude-zunsystem`。

```
model              claude-fable-5-1
max_tokens         64000
thinking           {"type":"adaptive","display":"summarized"}
output_config      {"effort":"low"}
context_management {"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}
stream             true
messages           1307 件 (user 654 / assistant 653)
system             3 block
tools              1 個 (Read のみ)
cache_control      0 個
```

**通常の cc request との差** (比較対象 `b9d30d68-….c88343cf.json`、同ディレクトリ):

| | compaction (`7d121f2e`) | 通常 (`c88343cf` 他) |
|---|---|---|
| system 末尾 block | `You are a helpful AI assistant tasked with summarizing conversations.` | `You are an interactive agent that helps users with software engineering tasks.` (以下フル system prompt) |
| tools | 1 (`Read`) | 29〜31 |
| cache_control | 0 | 3 (通常) / 31 (breakpoint を多く張った回) |
| output_config | `effort: low` | (無し) |

system は 3 block とも短い:

1. `x-anthropic-billing-header: cc_version=2.1.265.012; cc_entrypoint=cli;` (70 字)
2. `You are Claude Code, Anthropic's official CLI for Claude.` (57 字)
3. `You are a helpful AI assistant tasked with summarizing conversations.` (69 字)

kawaz の関心だった「圧縮された system prompt らしきもの」は、**圧縮版ではなく
要約タスク専用の最小 system prompt** だった。通常の巨大な cc system prompt
(ルール群・環境情報) は 1 文字も入っていない。会話の中身 (messages) 側に
`<system-reminder># Environment …` として入っているだけ。

`tools` に `Read` が 1 個だけ残っているのは奇妙で、最終 user message は逆に
「Read を含む一切のツールを呼ぶな」と強く禁じている。**推測**: Messages API が
`tools` 空配列を嫌う、あるいは cc の request 組み立てが最低 1 個を残す実装に
なっている。確認していない。

## 観測 2: 最終 user message = compact の指示文

messages[1306] (最後の user) は定型の compaction prompt だった。冒頭:

```
CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.
- You already have all the context you need in the conversation above.
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.

Your task is to create a detailed summary of the conversation so far, …
```

指定される出力構造は 9 節 (Primary Request and Intent / Key Technical Concepts /
Files and Code Sections / Errors and fixes / Problem Solving / All user messages /
Pending Tasks / Current Work / Optional Next Step)。セキュリティ関連の指示は
**verbatim で保持しろ**と明記されている (`These MUST be preserved verbatim in the
summary so they continue to apply after compaction.`)。末尾には
`## Compact Instructions` / `# Summary instructions` をユーザが与えている場合は
それに従え、という分岐もある。

## 観測 3: 誰の会話か — subagent だった

messages[0] の `<system-reminder>` に
`You are powered by the model named Opus 5 (1M context). The exact model ID is
claude-opus-5[1m].` があり、messages[1] は `ccmsg-webui` リポの issue を読む所から
始まる。これは親セッション (`49a66dfd`、ccmsg の統括) ではなく、統括が起動した
`rules-personal:opus5-worker-medium` の subagent `webui-first-connect` の会話。

transcript で裏が取れた:

```
~/.claude-personal/projects/-Users-…-ccmsg-main/49a66dfd-…/subagents/
  agent-awebui-first-connect-22f1409eaba15865.jsonl
  行 1654: {"type":"system","subtype":"compact_boundary","content":"Conversation compacted",
            "timestamp":"2026-09-13T14:58:07.914Z", "isSidechain":true}
  行 1655: user "This session is being continued from a previous conversation that …"
```

保持 request の `since_ms` = 1789311400536 = **2026-09-13T14:56:40Z**。
compact_boundary はその **87 秒後**。1307 messages の要約生成に要した時間として
自然で、因果は一致している。

**注目点**: subagent 自身は `claude-opus-5[1m]` で走っているのに、その会話を
要約する request の `model` は `claude-fable-5-1` (= 親セッションのモデル)。
`output_config.effort` も `low`。**推測**: compaction は subagent のモデルではなく
セッションのモデルで、かつ低 effort で回される。1 サンプルなので仕様の主張はしない。

親セッション側の transcript (`49a66dfd-….jsonl`) には同時刻に compact_boundary が
無く、14:55〜14:58 は「worker から段 2 完了の報告 → v0.17.0 を push」という通常の
やり取りが続いている。**親は compact していない**。

## 観測 4: タイミング — 各ターンではなく compact 発火時だけ

replay ログは 55 分間隔 (`since_ms` + 約 55 分から開始):

```
2026-09-13T15:52:04Z replayed … prefix=7d121f2e … cache="none"
2026-09-13T16:47:15Z … 17:42:26Z … 18:37:38Z … (以後 55 分ごと)
```

同じセッション `49a66dfd` には **通常形の保持 request も別 prefix で存在する**
(`cb4dee98`: msgs 1653 / tools 31 / cache_control 3、`since` 09-14 11:35 JST)。
つまり compaction request は「最後の request」としてたまたま保持された 1 本で、
毎ターン出るものではない。**compact が走った瞬間にだけ出る。**

## 観測 5: none 系列 4 本は同種ではない

ログ上 `cache="none"` だったのは 4 prefix。現物の json が残っていたのは 1 本だけ:

| prefix | session | route | json | compact_boundary | 判定 |
|---|---|---|---|---|---|
| `7d121f2e` | 49a66dfd (personal / ccmsg) | claude-zunsystem | **有り** | subagent に有り (時刻一致) | compaction で確定 |
| `48b85fe7` | d9a14568 (業務面) | claude-emrd | 消失 (lock のみ) | **無し** | 不明 |
| `78937d64` | e64c3fb9 (業務面) | claude-emrd | 消失 | **無し** | 不明 |
| `96c83ec2` | b507b442 (personal / rules) | claude-kawazzz | 消失 | **無し** | 不明 |

**「4 本とも compaction」という推定は成り立たない。** 残り 3 本は該当セッションと
その全 subagent transcript を走査しても `compact_boundary` が 1 件も無く、
推定送信時刻 (最初の replay の約 55 分前) 付近の transcript には普通の対話が
記録されている。つまり `cache_control` を持つ通常 request でありながら
cache に乗らなかった可能性が高く、**7d121f2e とは別の原因**を疑うべき。
現物が消えているので本調査では確定できない。

判定そのものは `crates/llm-gateway/src/gateway.rs:1128` の
`events::Cache::of(usage.as_ref())` で、応答 usage の cache read / creation が
0 なら `none`。保持 body に `cache_control` が無ければ必然的に `none` になる
(= 7d121f2e は構造的に説明が付く) が、3 本はその説明が使えない。

## なぜ cache_control が無いのか

**事実**: 保持された body に `cache_control` は 1 個も無い。`anthropic-beta` には
`prompt-caching-scope-2026-01-05` と `context-management-2025-06-27` の両方が
入っている (通常 request と同じヘッダ列)。

**推測** (裏は取っていない): compaction は 1 会話につき 1 回きりの使い捨て
request で、同じ prefix が再送される見込みが無い。cache 書き込みは通常
read より高価なので、再利用されない prefix に breakpoint を張るのは損になる。
cc がそれを避けているとすれば辻褄が合う。

**この系列に対する replay は原理的に無意味**である点だけは確定と言える。
cache_control が無い body を送り直しても、延命する cache entry がそもそも存在
しない。DR-0027 の replay 側で「`cache_control` を持たない保持 request は
そもそも保持・発火しない」ガードを検討する余地がある (本調査では提案まで)。

## 不明として残ったもの

- 3 本 (`48b85fe7` / `78937d64` / `96c83ec2`) が cache に乗らなかった理由。
  現物が消えているため確認不能。再発時に保持ファイルを退避できれば切り分けられる
- compaction request の `tools` に `Read` が 1 個だけ残る理由
- compaction のモデル選択規則 (親セッションのモデル固定か、別の規則か)。サンプル 1
- 親セッションの auto-compact でも同じ形 (tools 1 / cache_control 0) になるか。
  今回観測できたのは subagent の compact のみ

## 一次資料

- 保持 request: `~/.local/state/llm-gateway/stats/keepalive/49a66dfd-8ae0-4388-bdc1-a28c9960308e.7d121f2e.json`
- subagent transcript: `~/.claude-personal/projects/-Users-kawaz--local-share-repos-github-com-kawaz-ccmsg-main/49a66dfd-8ae0-4388-bdc1-a28c9960308e/subagents/agent-awebui-first-connect-22f1409eaba15865.jsonl`
- 親 transcript: 同ディレクトリの `49a66dfd-8ae0-4388-bdc1-a28c9960308e.jsonl`
- gateway ログ: `~/.local/state/llm-gateway/logs/unstable.log`
