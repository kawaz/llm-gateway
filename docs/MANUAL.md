# llm-gateway Manual

> English | [日本語](./MANUAL-ja.md)

A reference for the HTTP endpoints and CLI commands that llm-gateway provides.

The endpoints fall into three groups.

| Group | Path | Purpose |
| --- | --- | --- |
| Forwarding | `/v1/...`, `/ns-{name}/v1/...` | Speaks the Anthropic Messages API as-is |
| Operations | `/llm-gateway/...` | Liveness, usage, statistics, observation |
| Re-authorization | `/llm-gateway/login...` | Redo OAuth from a browser |

The examples below assume the gateway listens on `http://127.0.0.1:8402`.

## Namespaces

Forwarding paths may carry a namespace prefixed with `ns-`.

- `/ns-personal/v1/messages` → namespace `personal`
- `/v1/messages` → the default namespace (`default`)

The `ns-` prefix exists so a namespace name can be told apart from the API path
(`/v1/...`) (DR-0006). The namespace segment is stripped before forwarding, so the
upstream never sees it. Naming a namespace that is not configured returns 404, with
the configured namespace names listed in the body.

Authentication is per namespace. Only a namespace with `auth_token` under
`[ns.<name>]` inspects the `Authorization` header; a mismatch returns 401
(`authentication_error`). A namespace without `auth_token` passes traffic through
unchecked — the boundary is expected to be drawn in front (tailnet / Caddy).

## Prompt cache strategy (`[[ns.<name>.cache]]`)

Each namespace can say how the `cache_control` of a forwarded body is treated
(DR-0024). Rules are ordered like `routing` — model globs, first match wins — and
are matched against the model name after aliases are resolved.

```toml
[[ns.personal.cache]]
models = ["claude-fable-5-1*"]
main = "keepalive"
keepalive_horizon = "12h"

[[ns.personal.cache]]
models = ["*"]
main = "1h"
sub = "none"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `models` | (required) | Patterns this rule applies to. An empty list is a config error |
| `main` | `passthrough` | Strategy for requests from the main conversation |
| `sub` | `passthrough` | Strategy for requests from a subagent |
| `keepalive_horizon` | `8h` | How long `keepalive` keeps signalling a series |

The strategies:

| Value | Behavior |
| --- | --- |
| `passthrough` | The body is left alone |
| `none` | Every `cache_control` is stripped (for one-shot calls) |
| `5m` | Every breakpoint loses its `ttl` (= the default five minutes) |
| `1h` | Every breakpoint gets `ttl: "1h"` |
| `keepalive` | The body is written like `1h`, and when the conversation stops a signal goes out to draw one round trip that carries the cache into the next hour. **`main` only** — writing it under `sub` is a config error |

`keepalive_horizon` can be written two ways:

| Form | Example | Meaning |
| --- | --- | --- |
| Hours | `"12h"` | Twelve hours. A whole number followed by `h` |
| Share | `0.3` | Three tenths of the **break-even time**. Any number above 0 (1 and up is allowed) |

The break-even time is `(1h write rate / cache read rate) x 55 minutes` — the point
where signalling for that long costs as much as rebuilding the cache once. It follows
from the model's prices alone (80 pings, 73.3 hours, for Fable 5.1; 20 pings, 18.3
hours, for Opus 5), so a share applies the same judgement to models that cost
differently (`0.3` is 22 hours on Fable 5.1 and 5.5 hours on Opus 5). Measured against
the last seven days, **0.2 to 0.35** is the useful range
(`scripts/keepalive-horizon-sim.py`).

A model the price table does not cover cannot turn a share into hours, so it falls back
to the default of eight hours; the startup log and `llm-gateway check` name those
combinations.

Only `cache_control` is touched; breakpoints are never added or moved. A request counts as a
subagent (`sub`) when its `metadata.user_id` carries `parent_session_id`. Without one,
the billing header in the first `system` block decides: any `cc_entrypoint` other than
`cli` (`sdk-cli`, from `claude -p`) is a one-shot call (`oneshot`), while `cli` or no
header at all is the main conversation (`main`). A caller that cannot be read
(`unknown`) is treated as the main conversation. **The `sub` side applies to both `sub`
and `oneshot`** — neither is continued, so treating them as a conversation to come pays
nothing back.

`keepalive` can only reach a conversation through the `webhook` destination. With
no `webhook.base_url` configured no signal is raised (so it behaves exactly like
`1h`), and `llm-gateway check` lists that namespace as a warning.

### Running keepalive

The watch over a stopped conversation is kept in
`<stats dir>/keepalive/<listener>.json` and picked up again after a restart
(`stats.dir` defaults to `~/.local/state/llm-gateway/stats`). Each listener writes its
own file, so several processes may share one directory.

With several processes behind a load balancer, the signal **converges on a single one
from observation alone** (a process that sees another's signal steps back). Prefer a
priority policy (Caddy's `lb_policy first`) or sticky routing keyed on the
`X-Claude-Code-Session-Id` header; round-robin also converges, but with more duplicate
signals along the way.

## Forwarding

### `POST /{ns}/v1/messages`

Relays to the Anthropic Messages API unchanged. The response is streamed without
buffering, so `"stream": true` SSE passes straight through.

- Authentication: as configured for the namespace
- Request body limit: 64 MiB
- `model` selects a route according to the namespace routing, and is resolved to a
  real model name where needed before being handed to the upstream

When the namespace configures `thinking_display`, `thinking.display` is overridden —
but only for requests where the client stated `thinking` (DR-0016). Requests without
`thinking`, with `thinking.type = "disabled"`, with `tool_choice` of `any` / `tool`,
or ending in an assistant message (prefill) are left completely untouched.

```bash
curl -sS http://127.0.0.1:8402/ns-personal/v1/messages \
  -H 'authorization: Bearer <token>' \
  -H 'content-type: application/json' \
  -d '{"model":"opus","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}'
```

The upstream response is returned as-is (`{"type":"message","content":[...]}`). When
the gateway itself refuses, it answers with JSON in the Anthropic error shape.

```json
{"type": "error", "error": {"type": "invalid_request_error", "message": "..."}}
```

Errors:

| Situation | Status | `error.type` |
| --- | --- | --- |
| Namespace token mismatch | 401 | `authentication_error` |
| Body unreadable / not JSON | 400 | `invalid_request_error` |
| Unknown namespace | 404 | `invalid_request_error` |
| Model on no route | 404 | `not_found_error` |
| Every route failed / upstream unreachable | 503 | `api_error` |

### `POST /{ns}/v1/responses`

The entry point for clients that speak the Responses API (the codex CLI, DR-0025).
The body is passed to the upstream untouched apart from resolving the `model` alias,
and the answer is streamed back without translation. The only thing the gateway
replaces is authentication — the client's `Authorization` is dropped and the route's
`codex_oauth` credential supplies the token and `chatgpt-account-id`.

- Authentication: as configured for the namespace (whatever token the client names
  never reaches the upstream)
- Only routes that speak the Responses API (`provider = "openai"`) are eligible. When
  the model has no such route the answer is 404 (`no route for model ... can carry a
  responses request`) — deliberately worded apart from "model on no route", because
  the same model may well be reachable through `/v1/messages`
- Prompt cache strategies (`[[ns.<name>.cache]]`) do not apply. `cache_control`
  belongs to the Messages shape and has nowhere to live in this body
- Events (`/llm-gateway/events`) carry `origin: "codex"`
- Spending shows up in `/llm-gateway/stats` and `/llm-gateway/usage` exactly as it
  does for Messages traffic

#### Pointing the codex CLI at the gateway

Add a custom provider to `~/.codex/config.toml` (or `CODEX_HOME/config.toml`). The
gateway discards whatever `env_key` resolves to, so **any value works unless the
namespace sets `auth_token`** (in which case use that token).

```toml
model = "gpt-5.6-sol"
model_provider = "llm-gateway"

[model_providers.llm-gateway]
name = "llm-gateway"
base_url = "http://127.0.0.1:8402/ns-personal/v1"
env_key = "LLM_GATEWAY_API_KEY"
wire_api = "responses"
```

The provider id cannot be one of the codex CLI's built-in ids such as `openai`.

```bash
LLM_GATEWAY_API_KEY=dummy codex exec -m gpt-5.6-sol --skip-git-repo-check 'hi'
```

The codex CLI sends a `Session-Id` header, so route affinity works the same way as it
does for Messages traffic.

### `POST /{ns}/v1/messages/count_tokens`

The same relay as `/v1/messages`. Asks the upstream for a token count estimate.

```bash
curl -sS http://127.0.0.1:8402/v1/messages/count_tokens \
  -H 'content-type: application/json' \
  -d '{"model":"opus","messages":[{"role":"user","content":"hi"}]}'
```

### `GET /{ns}/v1/models`

The models available from that namespace, as shown in a client's model picker.
What is visible depends on the namespace routing configuration.

- Authentication: as configured for the namespace

```bash
curl -sS http://127.0.0.1:8402/ns-personal/v1/models
```

```json
{"object": "list", "data": [{"id": "claude-opus-5", "object": "model", "type": "model"}]}
```

## Operations

These live under `/llm-gateway/` so they never collide with upstream API names
(DR-0006). None of them carry authentication; the boundary is drawn in front.

**Every moment these endpoints report is a single number in Unix milliseconds.**
The same instant is never spelled a second way, so rendering it for a human is the
receiver's job. Only fields whose name carries a unit (`window_seconds`,
`cache_ttl_secs`) are *durations*, and those are in seconds.

### `GET /llm-gateway/healthz`

Reports only that the process is alive. Touches neither credentials nor upstreams.
Intended to be polled by a load balancer every few seconds.

```bash
curl -sS http://127.0.0.1:8402/llm-gateway/healthz   # => ok
```

### `GET /llm-gateway/version`

Reports the version **this process is running**, not what is installed on disk
(`{"version": "0.44.0"}`). Comparing the two is how `llm-gateway version` tells that a
binary was replaced but never restarted.

```bash
curl -sS http://127.0.0.1:8402/llm-gateway/version   # => {"version":"0.44.0"}
```

### `GET /llm-gateway/usage`

Per-credential usage (DR-0007). It reports utilization, reset times, and denial
state — never tokens or organization ids.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `refresh` | absent | Only `true` / `1` starts an active probe |

By default it reports only what was read by riding along with forwarded traffic,
so that **checking usage does not itself consume usage**. With `?refresh=true`, idle
credentials are sent a minimal request to be read again, and what that cost is
recorded under `probe`.

```bash
curl -sS 'http://127.0.0.1:8402/llm-gateway/usage?refresh=true'
```

```json
{
  "generated_at": 1785326400000,
  "probe": {"requests": 2, "model": "claude-haiku-4-5", "input_tokens": 18, "output_tokens": 1},
  "credentials": [
    {
      "name": "personal",
      "type": "claude_oauth",
      "support": "observed",
      "auth": {
        "status": "relogin_required",
        "reason": "log in again",
        "login_path": "/llm-gateway/login/personal/start",
        "observed_at": 1785326390000
      },
      "snapshot": {
        "observed_at": 1785326390000,
        "5h": {"utilization": 0.71, "status": "allowed", "reset": 1785340800000, "window_seconds": 18000},
        "7d": {"utilization": 0.34, "status": "allowed"}
      }
    }
  ]
}
```

`denials` holds the denials the gateway is currently honoring, with their reason and
scope (DR-0020). `limits` holds quotas asked for through the quota API, kept separate
from the header-derived `snapshot` (the two do not necessarily describe the same
quota). Fields with nothing to report are omitted entirely. When `auth.status` is
`relogin_required` for a `claude_oauth` credential, `auth.login_path` gives the relative
Web login page. `auth.status` is `org_not_allowed` when the upstream refuses OAuth use
for the whole organization: the login still works, so no `login_path` is offered, and
`auth.hint` carries a guess at the cause (an inactive subscription is one) rather than a
verdict.

### `GET /llm-gateway/status`

The official and observed state of the configured upstream services (DR-0021).

| Parameter | Default | Meaning |
| --- | --- | --- |
| `refresh` | absent | `true` / `1` re-fetches the official sources first |

```bash
curl -sS 'http://127.0.0.1:8402/llm-gateway/status?refresh=true'
```

```json
{
  "schema_version": 2,
  "generated_at": 1785326400000,
  "overall": {"severity": "ok", "service_counts": {"ok": 2, "warning": 0, "critical": 0, "unknown": 0}},
  "services": [
    {
      "id": "anthropic",
      "name": "Anthropic",
      "severity": "ok",
      "routes": ["personal"],
      "official": {
        "state": "operational",
        "source": "anthropic",
        "source_url": "https://status.anthropic.com/",
        "observed_at": 1785326100000,
        "stale": false,
        "components": [],
        "incidents": []
      },
      "observed": {"state": "ok", "observed_at": 1785326390000, "last_success_at": 1785326390000}
    }
  ]
}
```

`official` comes from the vendor status page; `observed` is what this gateway itself
saw while forwarding. Both are shown so you can tell apart the cases where the
official page says operational but traffic is not getting through — or the reverse.

### `GET /llm-gateway/stats`

Daily usage totals (DR-0011): token counts per day × credential × model, plus a USD
figure wherever a price table covers the model. What was written is never kept.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `days` | `7` | Limit to the last N days. `0` for everything |

The default is 7 days because returning everything makes the response grow with time.
A value that cannot be read as a number returns 400.

```bash
curl -sS 'http://127.0.0.1:8402/llm-gateway/stats?days=3'
```

```json
{
  "generated_at": 1785326400000,
  "days": {
    "2026-07-29": {
      "credentials": {
        "personal": {
          "claude-opus-5": {
            "requests": 12,
            "input": 1200,
            "output": 340,
            "input.cache_read": 8800,
            "usd": 0.42
          }
        }
      },
      "total_usd": 0.42
    }
  },
  "total_usd": 1.13
}
```

`total_usd` sums only the models the price table covers. If not a single row can be
priced, the field is omitted — so the number shown is never mistaken for the whole bill.

### `GET /llm-gateway/events`

Streams what happens on every forward, over SSE (DR-0012). You receive only what
happens **after** you connect; there is no replay. A slow watcher that misses events
does not stall the gateway (events are dropped and it moves on). A keep-alive is sent
every 20 seconds.

`Access-Control-Allow-Origin: *` is set, so a browser can open it directly to watch.
Neither bodies nor token counts are streamed.

```bash
curl -sSN http://127.0.0.1:8402/llm-gateway/events
```

```
event: request
data: {"ts":1785326400000,"session_id":"s-1","ns":"default","model":"claude-opus-5","credential":"personal","status":200,"prefix":"3f9a1c02","origin":"main","cache_ttl_secs":3600,"cache_expires_at":1785330000000,"cache_paused":false}
```

`prefix` is an 8-digit hash of the first block of the system prompt, marking which
conversation series a request belongs to; when it cannot be derived, the field is
omitted. `origin` says who asked (`main` / `sub` / `oneshot` / `unknown`; a request received in
the Responses shape is `codex`). `cache_ttl_secs` is
**how long the prefix this request leaves behind lives**, in seconds: it follows the
strategy that was applied, and for an untouched body it reads the `cache_control` that
was sent (3600 when any breakpoint carries `ttl:"1h"`, otherwise 300).
`cache_expires_at` is that moment. A request that leaves no breakpoint omits both. If
routes were skipped during route selection, `skipped` lists each credential and the
reason. A request that answered a cache signal carries `keepalive` (`applied` / `late`
/ `foreign` / `spent`).

A series watched by the `keepalive` strategy also carries the shape of its signal chain
(all omitted when no signal watches the series):

| Field | Meaning |
|---|---|
| `cache_since` | Where the chain starts: the last real request on this series |
| `next_keepalive_at` | When the next signal is due (omitted once no more go out) |
| `cache_count` | Which link this request is (a real request is 0, the k-th signal is k) |
| `cache_until` | **How far the signal can carry the cache**: when the hour the last signal buys runs out |
| `cache_until_count` | How many signals the chain will send in total |
| `cache_breakeven_until` / `cache_breakeven_count` | The same, counted to the break-even time (omitted when the model has no known price) |

Where `cache_expires_at` is when the hour this one request bought runs out, `cache_until`
is when the hour the *last* signal buys runs out. Signals go out every 55 minutes, and
the one that crosses `keepalive_horizon` is the last. If a signal cannot go out, the
cache dies earlier than this, so a watcher overwrites it with the latest notice.

Whether the signal is paused is reported in `cache_paused` (a bool) on **every** notice:
were the field omitted, "not paused" could not be told from "not reported".

When the response body closes, a second notice says how the turn ended.

```
event: response
data: {"type":"response","ts":1785326412000,"request_ts":1785326400000,"session_id":"s-1","prefix":"3f9a1c02","ns":"default","model":"claude-opus-5","credential":"personal","origin":"main","status":200,"stop_reason":"end_turn","aborted":false,"cache":"hit"}
```

`request_ts` is the `ts` of the matching `request` notice, so the two pair up one to
one even when several requests run on the same conversation. The identity fields
(`session_id` / `prefix` / `ns` / `model` / `credential` / `origin` / `status`) carry the
same values as that notice. `stop_reason` is whatever word the upstream used
(`end_turn` / `tool_use` / `max_tokens` …), passed through untouched; `end_turn` means
that client is back to waiting for input. `aborted` says whether the body failed to
reach its end (a client pressing Esc lands here) and is **always** present. A body that
was cut short says nothing about how it ended, so `stop_reason` is omitted there.

`cache` says how the prompt cache actually worked for this request: `hit` (the cache
that was there was read), `written` (nothing was left to extend, so the whole prefix was
written), `partial` (part was read and the rest written on top), `none` (this request
used no cache) or `unknown` (the usage could not be read). It is **always** present. The
`cache_expires_at` of the `request` notice is an estimate made before sending, so a
watcher overwrites it with this word. No token counts are included.

The notice goes out **only for the conversational endpoint** (`/v1/messages`). Counting
tokens (`/v1/messages/count_tokens`) is a forward but not a turn, so it streams nothing.
A request that did reach the conversational endpoint is announced even when the body was
cut before a single byte arrived (`aborted: true`). A dialect with no single word for
`stop_reason` (OpenAI) omits the field.

In a namespace using the `keepalive` strategy, a second kind of notice is streamed
when a conversation stops (DR-0024).

```
event: cache_keepalive
data: {"type":"cache_keepalive","ts":1785326640000,"session_id":"s-1","prefix":"3f9a1c02","nonce":"5Qv…","deadline":1785326670000,"marker":"[llm-gateway keepalive ping] nonce=`LLMGW-KEEPALIVE-5Qv…` — automated prompt-cache refresh from your own llm-gateway proxy (see llm-gateway docs, DR-0024). Reply with a single line containing only the nonce above, nothing before or after."}
```

The receiver injects `marker` verbatim into that conversation (`session_id`).
`nonce` is 32 random bytes as base64url — 43 characters — and `LLMGW-KEEPALIVE-`
followed by it is the token the answer consists of. The body of the
answer is treated like any other request (under `keepalive` every request writes the
hour); the notice only says whether it came back before `deadline` (`applied`) or after
it (`late`). A token **this gateway never minted** is reported as `foreign`: another
process watching the same conversation raised that signal, and this one steps back
(that is how several processes converge on a single signal, DR-0024). A token stays in
the conversation after the answer, so a later request of the same conversation may
carry it along; one already taken is reported as `spent` and the watch does nothing. While the route a conversation was cached on is unavailable, no signal is
raised at all. The same notice reaches the
`webhook` destination in the same shape.

Pausing the signal for a conversation (`POST /llm-gateway/keepalive/pause`) streams one
notice too. No further request arrives for a paused conversation, so this is the only
word that it stopped. A pause relayed from a sibling is not streamed (the same
destination would receive it twice). There is no notice for resuming: the real request
that lifts the pause carries `keepalive_paused: false`.

```
event: keepalive_paused
data: {"type":"keepalive_paused","session_id":"s-1","paused_at":1785326700000}
```

Every notice that promises a lifetime (a `request` carrying `cache_expires_at`, a
`cache_keepalive` carrying `deadline`) names that promise in `cache_notice`: 22
characters, freshly drawn per request, and equal to the signal's own `nonce` on a
`cache_keepalive`. When a promised lifetime ends without being kept — the cache died
first, across a suspend or a restart — a notice withdraws that name.

```
event: cache_expired
data: {"type":"cache_expired","ts":1785330001000,"session_id":"s-1","prefix":"3f9a1c02","of":"kUu1xR4-tQ9nSp2Zc0dBvA"}
```

A receiver keeps the latest `cache_notice` per (conversation, series) and zeroes its
countdown only when `of` matches it; otherwise it does nothing, because that promise has
already been replaced by a newer one. Several gateways can watch the same series without
confusing each other, since each names only its own promises. The end of the watching
window (`keepalive_horizon`) does not produce this notice: signalling merely stops, and
the cache the last signal bought lives on until `cache_until`.

To receive the same stream without holding a connection open (ccmsg on another host,
say), write the endpoint roots under `[webhook]` and the gateway POSTs to them. Both
`base_url` (one) and `base_urls` (several) are accepted, and **every notice goes to
every endpoint** — the gateway does not pick a destination. Conversation ids are
globally unique, so a receiver simply drops the conversations it does not know
(DR-0012). One endpoint being down does not stop delivery to the others.

```toml
[webhook]
base_url = "http://127.0.0.1:7777"
base_urls = ["http://192.168.1.5:7777", "http://192.168.1.6:7777"]
```

### `GET /llm-gateway/tap`

Streams the details of each forward as JSONL, one JSON object per line (DR-0017).
It is not SSE, so the output can be saved to a file and processed directly.

**Only direct loopback connections may use it.** Connections from anywhere else, and
connections carrying a `Forwarded` / `X-Forwarded-For` header, receive 403.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `include` | absent | `request_body` / `response_body`, comma-separated |
| `max_body` | `65536` | Byte limit at which an included body is truncated |

An unknown `include` value, an unknown parameter name, or a non-numeric `max_body`
returns 400. A subscriber that falls too far behind is disconnected (it does not
rejoin an older position in the stream).

```bash
curl -sSN 'http://127.0.0.1:8402/llm-gateway/tap?include=request_body,response_body&max_body=4096' \
  > tap.jsonl
```

```json
{"ts":1785326400000,"ns":"default","model":"claude-opus-5","route":"anthropic-a","status":200,"thinking":{"type":"adaptive"},"tool_choice":"auto","stream":false,"request_body_size":2481,"response_body_size":712,"credential":"personal","origin":"main"}
```

`origin` says who asked. `cache_strategy` appears when a prompt cache strategy was
applied, `cache_ttl_secs` when the request leaves a prefix behind, and `keepalive`
when the request answered a cache signal.
`request_body` / `response_body` appear only for subscriptions that asked for them,
and the truncation limit is independent per subscription. `thinking`, `tool_choice`,
and `stream` are the values the client sent, before the gateway rewrote anything.

## Re-authorization (login)

Endpoints for redoing OAuth from a browser once a refresh token has expired
(DR-0023). **Only `claude_oauth` credentials** are covered; for `codex_oauth` the page
points you at the CLI instead.

No extra authentication is placed here. The only thing that can be written is a token
that passed a genuine authorization — it is not an endpoint for writing arbitrary
values. Interception is prevented by state (CSRF) + PKCE + single-use TTL (10 minutes).

### `GET /llm-gateway/login`

An HTML page listing the credentials in the configuration. Each `claude_oauth` row
has only a "Log in" link to its credential-specific page. Each `codex_oauth` row
shows the CLI command to run.

```bash
open http://127.0.0.1:8402/llm-gateway/login
```

### `GET /llm-gateway/login/{name}/start`

Creates a state and a PKCE verifier, holds them in memory, and returns an HTML page for
that credential. The page shows the credential name, an Anthropic authorization link
that opens in a new tab, short instructions, and the code-paste form. After approval,
copy the `code#state` shown by the Anthropic console, return to the original page, paste
it, and save.

An unconfigured name returns 404, and a credential that is not `claude_oauth` returns 400.

### `POST /llm-gateway/login/{name}`

The receiving end of the paste flow. Send `application/x-www-form-urlencoded` with a
`code` field holding `code#state` (or a bare code with no `#`).

```bash
curl -sS http://127.0.0.1:8402/llm-gateway/login/personal \
  --data-urlencode 'code=<code>#<state>'
```

On success it returns HTML saying "Credential `<name>` was updated." An empty string,
input missing either half of the `#`, or an expired or already-used state returns 400.

Saving goes through the same path as the CLI login (it takes the credential lock and
writes back on top of what exists), so it never fights with the background refresh.

## CLI commands

```
llm-gateway <command> [options]
```

| Command | What it does | Matching endpoint |
| --- | --- | --- |
| `daemon` | The gateway processes of this installation (registry and running) | — |
| `service` | Registration with the operating system (the supervisor alone) | — |
| `upstream status` | Upstream service status (asks a running unit) | `/llm-gateway/status` |
| `check` | Read and verify the configuration (without starting) | — |
| `models` | List the models written in the configuration | — |
| `usage` | Usage per credential (asks a running unit) | `/llm-gateway/usage` |
| `stats` | Token counts and USD per credential × model × day | `/llm-gateway/stats` |
| `login` | Authorize in a browser and save to `<name>.json` | `/llm-gateway/login` |
| `version` | What is installed, what is running, and whether they differ | `/llm-gateway/version` |

Results are JSON (JSONL when following), errors are JSON on stderr with a non-zero
exit, and only the help is text — printed even with no arguments (DR-0028).

### `daemon` — running the units

A **unit** is one configuration file. It is registered under a name in
`$XDG_STATE_HOME/llm-gateway/daemon/units/<name>.toml`, and pointed at by that name
from then on. A unit holds the path of its configuration file and the path of the
binary to run (`[server] binary_path`, defaulting to whatever registered it).

| Command | What it does |
| --- | --- |
| `daemon run [unit]` | Run one unit in the foreground |
| `daemon supervise` | Run the supervisor in the foreground (it holds the registered units) |
| `daemon add <config>` | Register a configuration file as a unit (`--name <name>`) |
| `daemon remove <unit>` | Drop a unit from the registry |
| `daemon list` | List the registered units |
| `daemon start\|stop\|restart <unit>\|--all` | Ask the supervisor to move them |
| `daemon status [<unit>]\|--all` | How they are doing (`running` / `pid` / `version` / `restarts` / `last_exit`) |
| `daemon log [<unit>]\|--all` | Show what they wrote (`--follow` to keep reading) |

`start` / `stop` / `restart` / `status` ask the supervisor. If it is not running they
refuse with `supervisor_not_running` rather than starting a child themselves — otherwise
there would be no telling who owns the process. `restart --all` goes one at a time,
waiting for `/llm-gateway/healthz` before moving on.

### `version` — installed against running

`--version` prints one line: the version of the CLI you just ran. `version` prints JSON,
because there are two more versions worth knowing and they can disagree: what is
**on disk** (asked of the binary with `--version`, so what comes up next) and what is
**running** (asked of the live process, so what is being served right now).

```bash
llm-gateway version
{"cli":"0.44.0",
 "supervisor":{"running":"0.43.7","on_disk":"0.44.0",
               "binary_path":"/opt/homebrew/bin/llm-gateway","restart_needed":true},
 "units":[{"unit":"stable","running":"0.43.7","on_disk":"0.44.0",
           "binary_path":"/opt/homebrew/bin/llm-gateway","restart_needed":true}]}
```

`binary_path` is the file whose `--version` was read, because a version alone does not say
which of several installed binaries to replace. `restart_needed` is true only when both
versions are known and differ. A `null` means
nobody could answer — the supervisor is not running, the binary is gone, or an older
build without `/llm-gateway/version` is up — and that is not a reason to restart anything.
`supervisor` is null when nothing is registered with the operating system.

### `service` — registering with the operating system

What is registered is the supervisor (`daemon supervise`) alone. Which units it holds is
the registry's business, not the operating system's.

| Command | What it does |
| --- | --- |
| `service register` | Register the supervisor (launchd on macOS, systemd `--user` on Linux) |
| `service unregister` | Take it off again |
| `service start` / `stop` | Start or stop the registered supervisor |
| `service status` | Whether it is registered, whether it runs, and what it holds |
| `service log` | Show what the supervisor wrote (`--follow` to keep reading) |

`register --dry-run` prints the unit file it would write and the commands it would run,
and touches nothing. The Linux side is written but unverified (there is no systemd here).

`register` settles on the same shape however many times it is run: it does nothing when
the same unit is already loaded (`changed: false`), and swaps the unit in place when it
differs (`changed: true`). The binary it bakes in is the stable place on PATH that points
at this same binary (`/opt/homebrew/bin/llm-gateway` and the like); with no such place it
bakes in the running binary and adds a `warning` (`--executable <path>` names one).

The migration steps are in the
[runbook](./runbooks/2026-09-09-migrate-launchd-to-service.md).

Options:

| Option | Applies to | Meaning |
| --- | --- | --- |
| `--config <path>` | `check` / `models` / `daemon add` / `login` | Only the commands that take a configuration file itself |
| `--unit <name>` | `usage` / `stats` / `upstream status` | Which running unit to ask (the only one, if only one is registered) |
| `--name <name>` | `daemon add` | Name of the unit (default: the configuration file name without its extension) |
| `--all` | `daemon` start/stop/restart/status/log | Everything that is registered |
| `--follow` | `daemon log` / `service log` | Keep printing as more is written |
| `--dry-run` | `service register` | Print what would happen instead of registering |
| `--executable <path>` | `service register` | The binary to bake in (default: the stable path on PATH pointing at this same binary) |
| `--refresh` | `usage` / `upstream status` | Read again before showing (with `usage` this consumes a little) |
| `--days <N>` | `stats` | The last N days (default: 7, `0` for everything) |
| `--type <type>` | `login` | `claude_oauth` or `codex_oauth` |
| `--help`, `-h` | all commands | Show help |
| `--version` | — | Show the version |

Environment variables:

| Variable | Meaning |
| --- | --- |
| `LLM_GATEWAY_LOG` | Log verbosity (default: `info`) |
| `XDG_CONFIG_HOME` | Default location for the configuration |
| `XDG_STATE_HOME` | Default location for credentials, the unit registry, and logs |
