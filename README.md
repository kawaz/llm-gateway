# llm-gateway

> English | [日本語](./README-ja.md)

A thin LLM proxy that keeps clients from having to care about authentication.

```
ANTHROPIC_BASE_URL=http://127.0.0.1:xxxx
     ↓
llm-gateway ── OAuth pool     (resolves Claude subscription auth)
            ├─ Bedrock        (resolves the API key)
            └─ OpenAI family  (resolves ChatGPT subscription auth)
```

## What it does

A client (Claude Code and friends) only sets `ANTHROPIC_BASE_URL`. The gateway
resolves the subscription auth header or the Bedrock key and routes accordingly.
When you pick a `gpt-*` model, it connects to an OpenAI-family endpoint behind
the scenes.

There are four parts:

1. **Endpoint adapter** — the mouth that speaks the Anthropic Messages API
2. **Model backend router** — picks credentials and upstream from the model name, by priority
3. **Backend adapter** — absorbs the differences between upstreams
4. **Credential store** — acquires and refreshes tokens. Persistence is pluggable

## What it does not do

**Intervention is kept to a minimum.** The body is rewritten only in the `model`
field, which is the routing key; nothing else is touched. Headers get only the
generated credentials, plus removal of the `anthropic-beta` flags the upstream
rejects.

Its predecessor, CLIProxyAPI, impersonated Claude Code (beta flag injection /
cloak / device profile), and that turned into real breakage. On the Anthropic
route, subscription tokens have been measured to pass without any impersonation
(DR-0001).

There is no 429 detection → cooldown → failover either (measured: zero
occurrences). It will be added once it is actually needed.

## Status

**In service.** Forwarding (claude / codex / Bedrock), operational observation
(usage / stats / upstream status / tap), and web re-authentication are
implemented (details in [docs/MANUAL-ja.md](./docs/MANUAL-ja.md), Japanese).

The CLI owns the resident processes. One config file is registered as a unit
(`llm-gateway daemon add`), and a single supervisor holding those units is put
on the OS (`llm-gateway service register`, DR-0028).

## Documentation

- [docs/MANUAL-ja.md](./docs/MANUAL-ja.md) — HTTP API / CLI reference (Japanese)
- [docs/decisions/INDEX.md](./docs/decisions/INDEX.md) — decision records (DR, Japanese)
- [docs/QUESTIONS.md](./docs/QUESTIONS.md) — awaiting a ruling or a confirmation (Japanese)

The measurements and research behind all of this live in **`kawaz/llm-notes`**
(private), which is the source of truth:

- `docs/findings/2026-07-27-thin-proxy-poc.md` — the conditions under which writing our own works
- `docs/findings/2026-07-27-bedrock-api-key-integration.md` — the Bedrock route
- `docs/decisions/DR-0002-thin-proxy-design.md` — the decision to write our own

## License

MIT License, Yoshiaki Kawazu (@kawaz)
