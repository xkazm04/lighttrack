# lt-gateway — one endpoint in front of the seat-metered CLIs

`lt-gateway` is an OpenAI-compatible HTTP endpoint on localhost that local apps call instead of
carrying their own LLM wrapper. It routes a **use case** to a target, calls the engine's provider
dispatch (`claude -p` on a Claude seat, `codex exec` on a ChatGPT seat, or the HTTP providers),
fails over to the next seat when one is exhausted, and records every attempt in LightTrack.

Why it exists: three apps had three wrappers, and each shipped the same bugs — a failed primary
absorbed by retry-and-fallback and logged as a success on the model that did *not* fail, a
truncated body recorded as healthy, no `max_tokens`. The gateway is that wrapper written once,
measured with the benchmark framework, and honest to the store.

## What each provider path can take
The engine's input is a [`ChatRequest`] — system + turns (+ tools) — and each provider honours
what it can, refusing the rest **before** any request rather than dropping it silently:

| Target | Turns | Tools | Streaming |
|---|---|---|---|
| `openai`, `openrouter` (OpenAI wire shape) | native message array, including assistant `tool_calls` and `tool` results | **passed through verbatim**; the answer comes back as `tool_calls` with `finish_reason: tool_calls` | chunked delivery |
| `google` (Gemini), `anthropic` with `ANTHROPIC_API_KEY` | native `user`/`model` (`assistant`) turns | refused (400 names the target) | chunked delivery |
| `anthropic` via `claude -p`, `codex` | **rendered into one prompt** with role labels; the response says so (`lighttrack.transcript: true`) | refused | chunked delivery |

- **Tools on a route are checked up front, for every target in the chain.** A route whose
  fallback cannot run tools would otherwise serve tool-less prose during a limit window and call
  it a fallback; the gateway refuses the request instead and names the target.
- **Streaming is delivery, not generation.** `stream: true` returns `text/event-stream` with
  `chat.completion.chunk` events — a role chunk, the content split on whitespace, a
  `finish_reason` chunk, then a trailing usage chunk carrying the `lighttrack` block, then
  `[DONE]`. Nothing arrives before the model is done: the CLIs answer whole, and the HTTP paths
  are not streamed upstream either. What it buys is that an SDK call written against a streaming
  endpoint keeps working.
- **Not the judge's path.** The scoring engine keeps calling providers directly and stays
  unbudgeted. Routing the judge through here would put it under the app's limits and mix its
  spend into the app's rows.

## Running it
```
cargo build -p lighttrack-gateway
LIGHTTRACK_URL=http://127.0.0.1:8787 LIGHTTRACK_KEY=<project key> lt-gateway
```

| Env | Default | Meaning |
|---|---|---|
| `LIGHTTRACK_GATEWAY_BIND` | `127.0.0.1:8790` | bind address — keep it loopback; the endpoint has no auth of its own |
| `LIGHTTRACK_GATEWAY_CONFIG` | `gateway.toml` | routes (see `gateway.example.toml`); missing = literal targets only |
| `LIGHTTRACK_URL` / `LIGHTTRACK_KEY` / `LIGHTTRACK_PROJECT` | — | where events go; key **or** project (dev-mode API) must be set for telemetry and admission, else both are off |
| `LIGHTTRACK_GATEWAY_OMIT_CONTENT` | off | `1` never sends prompts/outputs on events (the project's redaction policy still applies on the API side) |
| `LIGHTTRACK_GATEWAY_DEV` | off | `1` honours `X-LightTrack-Simulate: exhausted:<provider>` for failover drills |
| `LIGHTTRACK_CLAUDE_BIN` / `LIGHTTRACK_CODEX_BIN` | auto | CLI overrides; the Windows npm shims are resolved automatically |

`.env` in the current directory is loaded, so run it from the app's repo.

## The request
`POST /v1/chat/completions`, the OpenAI shape. `model` is a **route name** from `gateway.toml` or a
literal `provider/model[@effort]`. `messages` (text parts only — no images), `tools` /
`tool_choice`, `response_format` (`json_schema` is enforced through the engine's schema path;
`json_object` becomes a system instruction), `stream`; anything else an SDK sends (`temperature`,
`max_tokens`…) is accepted and ignored — the CLIs have no such knobs. `X-LightTrack-Use-Case:
<name>` files a literal request's events under a use case.

The response is a `chat.completion` with `model` = the target that answered, real `usage` (with
`reasoning_tokens` where the provider reports them), and a `lighttrack` block: `route`,
`served_by`, `fell_back`, `cost_usd` (null on a seat), `determinism`, `schema` (whether the schema
actually reached the model), and `attempts` — every target tried, with its outcome. The same
facts ride on `x-lighttrack-served-by` and `x-lighttrack-fell-back` headers.

`GET /v1/models` lists the routes; `GET /health` shows which seats are on hold.

## Failover
Each failure is classified by what it says about the **seat**, not the call:

| Verdict | When | What happens |
|---|---|---|
| `exhausted` | a 429, a CLI stderr naming a usage limit or a logged-out session, an auth failure, a stated wait longer than the budget | next target, **and the seat is held out** for `cooldown_secs` (or the provider's stated retry-after) — later calls skip it (`skipped_cooling`) until the hold expires or a call succeeds |
| `transient` | 5xx, timeout, transport, empty completion, a crashed child | next target, no hold |
| `terminal` | a rejected schema, an unparseable envelope, a bad posture | stop — another seat would fail the same way, and trying it would spend a second seat on one bad request |

When every target fails, the response is `503` (with `Retry-After`, when a hold is the reason) or
`502`, in the OpenAI error shape plus `lighttrack.attempts`.

A route's fallback must be on a different seat from its primary; the config refuses otherwise.
`codex` is treated as the OpenAI seat for that rule.

## Admission
When telemetry is configured, each call first asks `GET /v1/limits/status` (cached 10s) whether
the project's enforcing rules are breached, and refuses with `429` **before** any seat is spent.
This is the inline pre-spend block that ingest-side admission could never do. Fail-open: an
unreachable API admits the call and says so on stderr.

## What it records
One event per attempt that actually called a seat, all sharing a `trace_id` per request:

- a failed attempt → `status: error` on the model that failed, tag `provider_failed`,
  `metadata.failure_class` = `transient` (chain moved on) or `terminal`;
- the answer → `status: success` on the model that answered, tag `fell_back` when it was not the
  first target;
- `metadata.gateway` carries the route, attempt index, chain length and which targets were
  skipped on hold. `source` is `lt-gateway`.

Cost comes from the CLI envelope for Claude (which includes the CLI's own auto-loaded context —
a per-call floor of roughly a cent or two) and is `null` for Codex, which reports none; the API
prices the latter from the price book by tokens if the model is in it.

## Choosing a route
`/gateway-onboard <app> <use-case>` is the measured way: it builds a difficulty-graded corpus for
the use case, benchmarks a small ladder on each seat, picks the cheapest configuration per seat
that clears the hard tier, and writes the route with the other seat as fallback only when it
clears the same bar. See `.claude/skills/gateway-onboard/SKILL.md`.
