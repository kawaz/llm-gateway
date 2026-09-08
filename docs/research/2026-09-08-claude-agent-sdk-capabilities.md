# Claude Agent SDK の能力と CLI 対応

- Date: 2026-09-08
- Status: Concluded

## 動機

Claude Agent SDK をホームスピーカー用途の常駐セッション（部屋ごとに 1 セッションを持ち、外部から prompt を投入して応答を取り出す）に使えるか判断するため、公開 API、Claude Code CLI との対応、権限・拡張機構、セッション永続化を公式資料・配布物・実機で確認した。

## 調査範囲

TypeScript `@anthropic-ai/claude-agent-sdk` 0.3.263 と同梱 Claude Code 2.1.263 を対象にした。Python SDK は API 名の棚卸しに留め、PoC は TypeScript で実施した。作業場所は `/private/tmp/sdk-research`、`CLAUDE_CONFIG_DIR=$HOME/.claude-bare`、model は `sonnet`、通信は llm-gateway 経由である。

## 調査メモ

### 2026-09-08: SDK の構造

Agent SDK は Claude Code の agent harness をライブラリから駆動する製品である。TypeScript 版の `query({ prompt, options })` は同梱 CLI を子プロセスとして起動し、stdin/stdout の stream-json と control protocol を型付き API に包む。Claude API の Tool Runner や Managed Agents とは別製品であり、実行基盤は利用者が運用する。

配布物で確認した版:

| 項目 | 値 |
|---|---|
| npm package | `@anthropic-ai/claude-agent-sdk` 0.3.263 |
| Node.js 要件 | `>=18.0.0` |
| 同梱 Claude Code | 2.1.263 |
| 同梱 CLI build | commit `37ae3f38d765199d54a6913cd61c6c9ad8576cc6`、2026-09-06 build |

`manifest.json` の `sdkCompat.testedWrapperVersions` は 0.3.223〜0.3.227 を列挙しており、これは同梱 CLI の互換試験メタデータであって「現在の wrapper が 0.3.227」という意味ではない。

### API 棚卸し

#### 入出力と複数ターン

```ts
query({
  prompt: string | AsyncIterable<SDKUserMessage>,
  options?: Options,
}): Query
```

`Query` は `AsyncGenerator<SDKMessage, void>` であり、`for await` で `system/init`、assistant content、tool use/result、turn ごとの `result` などを受け取る。`result` の `result` field が最終 assistant text、`permission_denials` が権限拒否の確定記録である。

- string input: 1 prompt を送り stdin を閉じる one-shot
- `AsyncIterable<SDKUserMessage>`: 同じ CLI process に複数 prompt を送り、各 turn の event を同じ iterator から得る
- `continue: true`: cwd の最新 conversation を継続（`resume` と排他）
- `resume: sessionId`: 指定 session を永続 transcript から再開
- `forkSession: true` + `resume`: 元 session を変更せず新 session ID へ fork
- `forkSession(sessionId)`: transcript store を直接 fork する別 API もある
- `persistSession`: 既定 `true`。`false` は disk 保存を止め、resume 不可
- `Query` control methods: `interrupt()`、`setPermissionMode()`、`setModel()`、`setSettings()`、`setMcpServers()`、`rewindFiles()` 等。control methods は streaming input/output 時だけ利用可能

現行 TypeScript 版には `listSessions`、`getSessionInfo`、`getSessionMessages`、`listSubagents`、`getSubagentMessages`、`renameSession`、`deleteSession` もある。`sessionStore` による transcript の外部 store dual-write は alpha である。

#### 権限

`canUseTool(toolName, input, options)` は Promise で次を返す。

```ts
{ behavior: "allow", updatedInput?, updatedPermissions? }
{ behavior: "deny", message, interrupt? }
```

重要なのは、callback が「すべての tool execution の直前」に無条件で呼ばれるわけではない点である。permission rules・mode・hook で事前に allow/deny が確定せず、host に確認する `ask` 経路になった時に呼ばれる。`permissionPrompts: "none"` なら callback は呼ばれず、prompt が必要な操作は即 deny される。`canUseTool` を渡すと CLI は `--permission-prompt-tool stdio` で起動する。

`permissionMode` は `default` / `acceptEdits` / `bypassPermissions` / `plan` / `dontAsk` / `auto`。`bypassPermissions` は `allowDangerouslySkipPermissions: true` も必要である。

#### hooks、custom tools、subagents

- `hooks`: `PreToolUse`、`PostToolUse`、`PermissionRequest`、`SessionStart`、`Stop`、`SubagentStart` など多数の event に async callback を登録できる。callback は SDK host process 内で実行され、control response として CLI に返る。
- custom tool: `tool(name, description, zodShape, handler)` と `createSdkMcpServer({name, tools})` で in-process MCP server を作り、`mcpServers` へ渡せる。別 process や socket server は不要。
- subagents: `agents: Record<string, AgentDefinition>` で prompt・tools・model・permission mode 等を programmatic に定義し、Agent tool から呼べる。`agent` は main thread 自体へ定義済み agent を適用する。

#### system prompt と settings

`systemPrompt` は次の形を取る。

- string / string[]: custom prompt
- `{type:"custom", prompt, snapshot}`
- `{type:"preset", preset:"claude_code", append?, excludeDynamicSections?, snapshot?}`

`snapshot: true` は、system-prompt recording が有効な環境では conversation の system prompt を transcript に一度記録し、後続 request と resume/continue で同じ byte sequence を再利用する契約である。SDK 型定義は prompt cache prefix と extended thinking の安定のためこれを推奨している。custom prompt の bare string は snapshot されないため、opt-in するには object form が必要である。ただし recording は account 単位で rollout 中であり、未有効の環境では option が受理されても no-op になる。今回の実測環境では no-op だった（後述）。

`settingSources` は `user` / `project` / `local`。省略時は全 source、`[]` は filesystem settings を読まない SDK isolation mode。`project` を含めないと CLAUDE.md も読まれない。`env` option は subprocess environment を置換し、merge しないため、通常は `{...process.env, ...overrides}` とする。

### CLI argv への対応

0.3.263 の minified `sdk.mjs` で argv builder を確認した。全 query は少なくとも次で起動する。

```text
--output-format stream-json --verbose --input-format stream-json
```

| SDK option | CLI / control protocol |
|---|---|
| `canUseTool` | `--permission-prompt-tool stdio` |
| `permissionPromptToolName` | `--permission-prompt-tool <name>` |
| `permissionPrompts` | `--permission-prompts <host|none>` |
| `continue` | `--continue` |
| `resume` | `--resume=<id>` |
| `forkSession` | `--fork-session` |
| `sessionId` | `--session-id=<id>` |
| `settingSources` | `--setting-sources=<comma-list>`（空配列は空値） |
| `permissionMode` | `--permission-mode <mode>` |
| `allowedTools` / `disallowedTools` / `tools` | 同名 CLI flag |
| serializable `mcpServers` | `--mcp-config <json>` |
| `strictMcpConfig` | `--strict-mcp-config` |
| `model` / `fallbackModel` | `--model` / `--fallback-model` |
| `includePartialMessages` | `--include-partial-messages` |
| `persistSession: false` | `--no-session-persistence` |
| `settings` | `--settings`（inline object は argv builder で flag settings 化） |
| `plugins`（既定 delivery） | plugin ごとに `--plugin-dir` |
| `systemPrompt` | argv flag ではなく initialize control request |
| `agents` | argv の `--agents` ではなく initialize control request |
| in-process MCP server / hooks | SDK control channel 上で登録・応答 |

`systemPrompt` / `agents` を argv で渡さないのは、関数や server instance を含む SDK 固有状態を initialize handshake で渡すためである。SDK は `CLAUDE_CODE_ENTRYPOINT` が未設定なら subprocess に SDK entrypoint を設定する。

### 認証、proxy、`CLAUDE_CONFIG_DIR`

PoC は `env: {...process.env, CLAUDE_CONFIG_DIR: "$HOME/.claude-bare"}` で成功し、既存の gateway 設定を継承した。したがって SDK subprocess は `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` を含む親環境を通常の Claude Code と同様に利用できる。`env` option を指定してこれらを落とすと継承されない。

`CLAUDE_CONFIG_DIR` は CLI subprocess が読む settings、credentials、transcript 保存先を切り替える。PoC の `system/init` session ID と transcript は bare config 側に作られ、process 終了後の `resume` も同じ config dir で成功した。別 config dir から同じ session を resume できるとは限らない。

### 実機 PoC

再現コードは `/private/tmp/sdk-research/poc.ts` に置いた。リポジトリには一時コードを含めない。

共通 option の要点:

```ts
const base = {
  cwd: "/private/tmp/sdk-research",
  model: "sonnet",
  settingSources: [],
  env: { ...process.env, CLAUDE_CONFIG_DIR: `${process.env.HOME}/.claude-bare` },
  permissionMode: "default" as const,
  systemPrompt: {
    type: "custom" as const,
    prompt: "Follow the user exactly. Keep replies minimal.",
    snapshot: true,
  },
};
```

#### A. 1 process 2 turn

`AsyncIterable<SDKUserMessage>` から nonce を含む 2 message を yield した。同じ session ID で turn ごとの `system/init` と `result` が返った。

```text
A INIT 41aa904c-1574-4210-94ae-b3b398f9a6f0
A TEXT "TURN1"
A RESULT success "TURN1" []
A INIT 41aa904c-1574-4210-94ae-b3b398f9a6f0
A TEXT "SDK-N7Q"
A RESULT success "SDK-N7Q" []
```

判定: 成功。応答 text は assistant message の text block、turn 完了値は `result.result` から取得できる。

#### B. `canUseTool` allow / deny

最初に bare settings の既存 allow が効く状態で試すと Bash は実行されたが callback は 0 回だった。次に `settings.permissions.ask: ["Bash(*)"]` を flag settings として明示して ask 経路を作った。

```text
B-allow TOOL Bash {"command":"printf SDK-BASH-OK",...}
B-allow TEXT "SDK-BASH-OK"
B-allow-CALLBACK [{"name":"Bash",...}]

B-deny TOOL Bash {"command":"printf SDK-BASH-OK",...}
B-deny TEXT "The Bash tool call was denied by SDK policy."
B-deny RESULT success ... [{"tool_name":"Bash",...}]
B-deny-CALLBACK [{"name":"Bash",...}]
```

callback の deny return は `{behavior:"deny", message:"SDK policy denied Bash for PoC"}`。この理由が tool error としてモデルへ返り、モデルは意味を保った最終文を生成した。権限拒否の構造化された確定記録は `result.permission_denials` に残った。

#### C. in-process custom tool

```ts
const server = createSdkMcpServer({
  name: "poc",
  tools: [tool("nonce_lookup", "Return fixed token",
    { label: z.string() },
    async ({ label }) => ({ content: [{ type: "text", text: `CUSTOM-${label}-42` }] }))],
});
```

```text
C TOOL mcp__poc__nonce_lookup {"label":"sdk"}
C TEXT "CUSTOM-sdk-42"
C RESULT success "CUSTOM-sdk-42" []
```

判定: 成功。handler は SDK host process 内で実行された。

#### D. process 終了後の resume

A の process が終了した後、別 `query()` に `resume: sessionId` を指定した。

```text
D INIT 41aa904c-1574-4210-94ae-b3b398f9a6f0
D TEXT "SDK-N7Q"
D RESULT success "SDK-N7Q" []
```

判定: 成功。永続 transcript から nonce context が復元された。

#### E. `PreToolUse` hook

Bash を allow し、`PreToolUse` matcher `Bash` の callback で invocation count と input を観測した。

```text
E TOOL Bash {"command":"printf HOOK-OK",...}
E-HOOK {"hook_event_name":"PreToolUse","tool_name":"Bash",...}
E TEXT "HOOK-OK"
E RESULT success "HOOK-OK" []
E-HOOK-COUNT 1
```

判定: 成功。hook input には session ID、cwd、permission mode、tool input、tool use ID が含まれた。

### `systemPrompt.snapshot` と gateway tap

`systemPrompt: {type:"preset", preset:"claude_code", snapshot: <条件>}` について、`true` / `false` / 未指定の 3 条件を比較した。各条件で 1 ターン目の後に同一 SDK process の cwd を `git init` で変化させて 2 ターン目を送り、その process を終了して同じ session ID を `resume` した。tap では今回だけの marker を最後の user message から特定し、同一 marker のうち `request_body_size` 最大の main request を選んだ。本文は保存せず、system block ごとの byte length、SHA-256、`cache_control` だけを記録した。

全条件で system は 3 blocks、`cache_control` は `[-, CC, CC]` だった。比較対象として変化を含む system[2] の観測値を示す。

| `snapshot` | 1 ターン目 | 同一 process の 2 ターン目 | process 終了後 `resume` |
|---|---|---|---|
| `true` | 27,907 bytes / `d9fc00cd3bc1…` | 27,907 / `d9fc00cd3bc1…` | 28,233 / `f715336af327…` |
| `false` | 27,909 / `38f2db324d91…` | 27,909 / `38f2db324d91…` | 28,235 / `1ea83d6205b9…` |
| 未指定 | 27,913 / `5bc89768617f…` | 27,913 / `5bc89768617f…` | 28,239 / `879e8be81104…` |

判定: **今回のアカウントでは `snapshot: true` は process 再起動を跨いで実効しなかった**。同一 process 内では 3 条件すべてが byte-for-byte 安定し、`resume` 後は 3 条件すべてで system[2] の長さと hash が変化した。したがって同一 process 内の安定は snapshot recording の効果ではなく、process 内で system prompt が固定される通常挙動である。これは型定義の「recording 未有効の account では option を受理しても no-op」という記述と一致する。

SDK main request の system 構造は CLI `-p` の実測と同じ 3 blocks・`[-, CC, CC]` だった。system[0] の請求ヘッダは次の形で、`cc_is_subagent` は無かった。

```text
x-anthropic-billing-header: cc_version=2.1.263.272; cc_entrypoint=sdk-cli;
```

system[1] は Claude Agent SDK host であることを示す短い block、system[2] は Claude Code の主要 system prompt である。SDK も CLI `-p` も `cc_entrypoint=sdk-cli` を使うため、billing header だけでは両者を区別できない。

なお、`CLAUDE_CONFIG_DIR=$HOME/.claude-bare` で起動した SDK request の tap event は `ns:"bare"` ではなく `ns:"personal"` だった。gateway namespace は `CLAUDE_CONFIG_DIR` と同一軸ではない。今回の request は namespace だけに頼らず固有 marker との積で同定した。

### 版と互換性

0.3.263 の型と同梱 2.1.263 の組み合わせでは全 PoC が成功した。`pluginDelivery: "initialize"` は型定義上 Claude Code 2.1.261 以上を要求する。`pathToClaudeCodeExecutable` で別 CLI を指定できるが、新しい initialize/control capability を古い CLI が持つ保証はないため、常駐サービスでは SDK package と同梱 CLI を組として pin するのが安全である。

破壊的変更の時系列は package 配布物に CHANGELOG が含まれず、今回網羅できなかった。型定義には deprecated 項目として `maxThinkingTokens`（`thinking` 推奨）や `allowedTools` 内の `Skill`（`skills` option 推奨）が明示されている。

## 暫定的な結論

ホームスピーカー用途では CLI を直接 spawn するより Agent SDK が適する。

- streaming input なら 1 process / 1 session を常駐させ、外部 prompt を async queue から yield し、構造化 event から応答を取り出せる。
- process crash・更新後も `resume` で conversation を回復できる。部屋ごとの session ID と `CLAUDE_CONFIG_DIR` を永続管理すればよい。
- permission UI は `canUseTool`、自前能力は in-process MCP、監査は hooks として同じ host process に実装できる。CLI の control JSON を直接実装する必要がない。
- `systemPrompt.snapshot: true` は system-prompt recording が有効な account では再起動を跨いだ prompt-cache prefix の安定に効くが、今回の account では no-op だった。常駐 process 内では指定値に関係なく system prompt が安定したため、現時点の cache 維持は process を常駐させる設計に依存する。
- `settingSources: []` は予期しないローカル設定・CLAUDE.md・hooks の混入を防ぐ。ただし必要な permission rules や tools は SDK option で明示する必要がある。

CLI 直叩きの利点は依存層が薄く wire protocol を完全に制御できる点である。一方、SDK は protocol version、control request、permission response、in-process MCP、session persistence を既に抽象化する。常駐サービスで CLI 直叩きを選ぶと SDK と同じ bridge を自前保守することになるため、CLI にしかない新機能の先行利用や SDK 外 protocol の研究でない限り得策ではない。

注意点として、`canUseTool` を「全 tool call interceptor」と見なしてはならない。無条件の監査は hooks、権限判断は permission rules と `canUseTool` の組み合わせで設計する。また string prompt は stdin を閉じる one-shot なので、常駐 input には必ず `AsyncIterable` を使う。

### 未検証

- Python SDK の同等 PoCと TypeScript 版との差分
- 公式 CHANGELOG 全期間の破壊的変更一覧
- `forkSession` / `continue` の実機 PoC
- programmatic `agents` option から subagent を起動する実機 PoC
- 長時間常駐時の stdin backpressure、再接続、compaction、prompt cache TTL と keepalive の実測

## 関連

- [Claude Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview)
- [TypeScript SDK reference](https://code.claude.com/docs/en/agent-sdk/typescript)
- [Python SDK reference](https://code.claude.com/docs/en/agent-sdk/python)
- [Sessions](https://code.claude.com/docs/en/agent-sdk/sessions)
- [Permissions](https://code.claude.com/docs/en/agent-sdk/permissions)
- [Hooks](https://code.claude.com/docs/en/agent-sdk/hooks)
- [Custom tools](https://code.claude.com/docs/en/agent-sdk/custom-tools)
- [Subagents](https://code.claude.com/docs/en/agent-sdk/subagents)
- [Hosting](https://code.claude.com/docs/en/agent-sdk/hosting)
- [稼働中セッションへの外部注入経路と JSON 口の実態](2026-09-08-session-injection-and-json-port.md)
