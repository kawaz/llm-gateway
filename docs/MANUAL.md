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

Only `cache_control` is touched; breakpoints are never added or moved. A request
counts as a subagent when its `metadata.user_id` carries `parent_session_id`;
a caller that cannot be read is treated as the main conversation.

`keepalive` can only reach a conversation through the `webhook` destination. With
no `webhook.base_url` configured no signal is raised (so it behaves exactly like
`1h`), and `llm-gateway check` lists that namespace as a warning.

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

### `GET /llm-gateway/healthz`

Reports only that the process is alive. Touches neither credentials nor upstreams.
Intended to be polled by a load balancer every few seconds.

```bash
curl -sS http://127.0.0.1:8402/llm-gateway/healthz   # => ok
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
  "generated_at": 1785326400,
  "generated_at_iso": "2026-07-29T12:00:00Z",
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
        "observed_at": 1785326390,
        "observed_at_iso": "2026-07-29T11:59:50Z"
      },
      "snapshot": {
        "observed_at": 1785326390,
        "observed_at_iso": "2026-07-29T11:59:50Z",
        "5h": {"utilization": 0.71, "status": "allowed", "reset": 1785340800, "window_seconds": 18000},
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
Web login page.

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
  "schema_version": 1,
  "generated_at": 1785326400,
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
        "observed_at": 1785326100,
        "stale": false,
        "components": [],
        "incidents": []
      },
      "observed": {"state": "ok", "observed_at": 1785326390, "last_success_at": 1785326390}
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
  "generated_at": 1785326400,
  "generated_at_iso": "2026-07-29T12:00:00Z",
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
data: {"ts":1785326400,"ts_iso":"2026-07-29T12:00:00Z","session_id":"s-1","ns":"default","model":"claude-opus-5","credential":"personal","status":200,"prefix":"3f9a1c02"}
```

`prefix` is an 8-digit hash of the first block of the system prompt, marking which
conversation series a request belongs to; when it cannot be derived, the field is
omitted. If routes were skipped during route selection, `skipped` lists each
credential and the reason. A request that answered a cache signal carries
`keepalive` (`applied` / `late`).

In a namespace using the `keepalive` strategy, a second kind of notice is streamed
when a conversation stops (DR-0024).

```
event: cache_keepalive
data: {"type":"cache_keepalive","ts":1785326640,"ts_iso":"2026-07-29T12:04:00Z","session_id":"s-1","prefix":"3f9a1c02","nonce":"5Qv…","deadline":1785326670,"deadline_iso":"2026-07-29T12:04:30Z","marker":"[llm-gateway cache keepalive] token=`LLMGW-KEEPALIVE-5Qv…`; reply with a single line containing only that token, nothing before or after"}
```

The receiver injects `marker` verbatim into that conversation (`session_id`).
`nonce` is 32 random bytes as base64url — 43 characters — and `LLMGW-KEEPALIVE-`
followed by it is the token the answer consists of. The body of the
answer is treated like any other request (under `keepalive` every request writes the
hour); the notice only says whether it came back before `deadline` (`applied`) or after
it (`late`). While the route a conversation was cached on is unavailable, no signal is
raised at all. The same notice reaches the
`webhook` destination in the same shape.

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
{"ts":1785326400,"ns":"default","model":"claude-opus-5","route":"anthropic-a","status":200,"thinking":{"type":"adaptive"},"tool_choice":"auto","stream":false,"request_body_size":2481,"response_body_size":712,"credential":"personal"}
```

`cache_strategy` appears when a prompt cache strategy was applied, and `keepalive`
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
| `serve` | Start listening | (all of them) |
| `check` | Read and verify the configuration (without starting) | — |
| `models` | List the models written in the configuration | — |
| `usage` | Usage per credential (asks the server) | `/llm-gateway/usage` |
| `status` | Upstream service status (asks the server) | `/llm-gateway/status` |
| `stats` | Token counts and USD per credential × model × day | `/llm-gateway/stats` |
| `login` | Authorize in a browser and save to `<name>.json` | `/llm-gateway/login` |

Options:

| Option | Applies to | Meaning |
| --- | --- | --- |
| `--config <path>` | all commands | Configuration file (default: `$XDG_CONFIG_HOME/llm-gateway/config.toml`) |
| `--refresh` | `usage` / `status` | Read again before showing (with `usage` this consumes a little) |
| `--days <N>` | `stats` | The last N days (default: 7, `0` for everything) |
| `--type <type>` | `login` | `claude_oauth` or `codex_oauth` |
| `--help`, `-h` | all commands | Show help |
| `--version` | — | Show the version |

Environment variables:

| Variable | Meaning |
| --- | --- |
| `LLM_GATEWAY_LOG` | Log verbosity (default: `info`) |
| `XDG_CONFIG_HOME` | Default location for the configuration |
| `XDG_STATE_HOME` | Default location for credentials and logs |
