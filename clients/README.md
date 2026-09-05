# LightTrack client SDKs

Thin, **fire-and-forget** libraries that send your apps' LLM calls to a LightTrack server. They wrap
an OpenAI / Anthropic / Gemini result, normalize it, and POST it to `/v1/events` — without blocking
your request path and without ever throwing into your app (telemetry must never break the caller).

The API fills in the rest: it derives the **project from the API key**, assigns the event id and
timestamp, and computes **cost** from its price book. So the minimal event is just
`{provider, model, usage}`.

## What each SDK can do

The three clients are not identical, and the table below is generated from each one's own
`lighttrack.manifest.json` rather than written by hand — so a gap shows up here instead of being
inferred from the silence of a README. CI regenerates it and fails if it has gone stale.

<!-- BEGIN GENERATED: sdk-capability-matrix (scripts/gen-sdk-matrix.mjs) -->

<!-- Generated from clients/*/lighttrack.manifest.json. Do not edit by hand: run
     `node scripts/gen-sdk-matrix.mjs`. CI fails if this block is stale. -->

| Capability | python | typescript | rust |
|---|---|---|---|
| Record one call (`track*`) | yes | yes | yes |
| Time a call (`span` / context manager) | yes | yes | no |
| Wrap the provider SDK (auto-capture) | yes | yes | no |
| Crash-surviving breadcrumbs | yes | yes | no |
| Inline output guardrails (`guard`) | yes | yes | yes |
| Relay tasks (cloud -> device) | yes | yes | no |
| Pre-spend admission on limits | yes | yes | yes |
| Client-side batching | no | no | no |

Where a cell is not `yes`, the SDK says why:

- **rust / Time a call (`span` / context manager)** (no): The builder emits only on send(); there is no timed span type, so latency is caller-measured.
- **rust / Wrap the provider SDK (auto-capture)** (no): No provider-SDK wrapping. Rust provider clients are third-party and un-monkey-patchable; instrumentation would need per-crate adapters.
- **rust / Crash-surviving breadcrumbs** (no): No crash-surviving breadcrumb. A Rust process killed mid-call leaves no record of it, where the Python and TypeScript clients would recover one. This is a gap, not a design choice - clients/catchup-marker.json has carried it as a flagged debt since 2026-08-24.
- **rust / Relay tasks (cloud -> device)** (no): No relay client. Enqueue tasks over plain HTTP against /v1/relay/tasks (docs/RELAY.md).
- **python / Client-side batching** (no): No client-side batching: events go one per POST from the background thread. /v1/events/batch exists server-side.
- **typescript / Client-side batching** (no): No client-side batching: sends are individual un-awaited POSTs. /v1/events/batch exists server-side.

<!-- END GENERATED: sdk-capability-matrix -->

They *do* share one contract. Provider extraction, `guard` verdicts, the journal format, the limit
signals on an ingest response, and the failure diagnostics are pinned by shared fixtures in
[`clients/contract/`](contract/) and asserted by all three suites, so a behaviour that exists in two
languages cannot quietly mean something different in the third. `guard`'s PII rules are exported from
the server's own scrubber (`crates/anon`), so a client-side check can no longer contradict what the
ingest path would redact.

## One-line auto-instrumentation

Don't want to write a `track*` per call? **Wrap the provider SDK once** and every call it makes is
captured automatically — model, token usage, latency, and trace linkage. Calls made inside a trace
share a `trace_id`; nested spans set `parent_span_id`, so multi-step / agentic apps feed straight
into the [trace view](../docs/DATA_MODEL.md). It stays best-effort: instrumentation never throws into
your app, and a failing provider call is still recorded (as a failed span) before its error rethrows.

```python
# Python — patch every installed SDK globally with a single import:
import lighttrack.auto
resp = openai_client.chat.completions.create(...)   # auto-tracked, no extra code

# ...or instrument just one client instance:
from lighttrack import wrap, trace
client = wrap(openai_client)
with trace():                                        # calls inside share one trace_id
    client.chat.completions.create(...)
```

```ts
// TypeScript — wrap a client instance (drop-in: same object back):
import { wrapOpenAI, withTrace, withSpan } from "lighttrack-client";
const openai = wrapOpenAI(new OpenAI());
await withTrace(async () => {
  await openai.chat.completions.create({ ... });     // auto-tracked (trace root)
  await withSpan(async () => {
    await openai.chat.completions.create({ ... });   // auto-tracked (child span)
  });
});
```

Wrapped methods: OpenAI `chat.completions` / `responses` / `embeddings`, Anthropic `messages`, and
Google GenAI `models.generate_content` (Python also patches the legacy `google.generativeai`
`GenerativeModel`). Streaming calls are recorded with latency + model (token usage isn't captured
from a stream yet). The hand-written `track*` / `span` API below still works for full control.
**Python and TypeScript only** — see the matrix above.

| Language   | Dir                 | Install / run                              | Notes |
|------------|---------------------|--------------------------------------------|-------|
| Python     | `clients/python`    | `pip install ./clients/python`             | stdlib only, background thread |
| TypeScript | `clients/typescript`| `npm install` (or vendor `src/index.ts`)   | global `fetch`, zero deps, Node 18+/browser |
| Rust       | `clients/rust`      | path/git dep on `lighttrack-client`        | reuses `lighttrack-core::LlmEvent` |

## Configuration (all three)

Read from the environment (or pass explicitly to the constructor):

- `LIGHTTRACK_URL` — API base URL (default `http://127.0.0.1:8787`).
- `LIGHTTRACK_KEY` — a project or admin key (`Bearer`). With a **project key**, the project is
  derived server-side. Empty values are ignored.
- `LIGHTTRACK_PROJECT` — project id to stamp on events. Needed when using an **admin key**, and the
  way to choose a project in dev mode; ignored when a project key already pins the project.
- `LIGHTTRACK_QUIET` — set to `1` to silence the SDK's failure diagnostics (see below).
- `LIGHTTRACK_ENFORCE` — `block` | `warn` | `off` (default `off`): pre-spend admission, below.

**Where do my events land?** Every event is attributed to a project. A **project key** pins it
server-side; otherwise the event has to name one. With neither, a **dev-mode** server files events
under a `default` project — fine for a first run — while a server with **authentication enabled**
rejects them. So set one of these as soon as you want events somewhere specific:

```bash
export LIGHTTRACK_URL=http://127.0.0.1:8787
export LIGHTTRACK_PROJECT=demo        # ...or export LIGHTTRACK_KEY=lt_... instead, and skip this
```

## Pre-spend admission (stop the call before it costs money)

Every cap the server has is **record-side**: it refuses to *record* a call that already cost money.
By the time a 429 arrives, the provider has been paid. The SDKs close that gap locally.

Each client keeps the last thing the server said about your project's caps — `usage_ratio`,
`shed_fraction`, the wait a 429 advertised, and which rule is binding — updated from every ingest
response it already makes. `admit()` answers from that cache with **no network call**, and `enforce`
decides what happens to a refusal:

```python
# Python
lt = LightTrack(enforce="block")           # or "warn" / "off" (default), or LIGHTTRACK_ENFORCE
lt.instrument()                            # every wrapped provider call is now gated
```

```ts
// TypeScript
const lt = new LightTrack({ enforce: "block" });
const openai = wrapOpenAI(new OpenAI(), lt);   // throws LightTrackBudgetExceeded instead of calling
```

```rust
// Rust — no wrapper to hide it in, so the gate is called directly
let lt = Client::from_env().enforce(Enforce::Block);
lt.gate(Some("summarize"))?;                   // returns before a token is bought
```

Three properties worth knowing:

- **Off by default.** Adding an observability SDK must not change what your app does. You opt into
  having it stop calls.
- **It fails open.** A client that has never reached the server, or whose view is older than 30s,
  admits. A telemetry client that halts your LLM calls because it is itself confused is worse than
  one that records nothing. A stale verdict triggers one background refresh from
  `GET /v1/limits/status`; the decision never waits on it.
  **Its worst case, stated:** the 30s bounds how long a *refusal* can rest on old evidence, not how
  long the client can keep spending. With the server reachable, a client learns that the cap was
  crossed from the next ingest response it gets, so the overshoot is the calls in flight during one
  send round-trip — about `ingest latency × your call rate`, one step behind the server's own cap,
  which is itself one call late. With the server unreachable the view goes stale and the client
  admits everything, by design; nothing here bounds that spend, which is what the provider's own
  spend ceiling is for.
- **A blocked call is not spend.** It is never recorded as cost. Pass `record_blocked` /
  `recordBlocked` to leave a zero-usage event tagged `lt_blocked_locally` instead, so your rollups
  show a throttled week rather than a quiet one.

The refusal is typed (`LightTrackBudgetExceeded` / `BudgetExceeded`) so your app can tell "the budget
said no" — degrade to a smaller model, a cached answer, a queue — from a provider outage, which is a
retry. Scoped caps stay scoped: a cap on the `summarize` use-case stops `summarize` and nothing else.

## Why don't I see my events? (SDK diagnostics)

Sends are best-effort and never throw — but they are **not silent**. When a send fails, each SDK
writes one actionable line to stderr (`console.warn` in TypeScript, never stdout), naming the likely
cause and the fix:

```
[lighttrack] event not sent to http://127.0.0.1:8787/v1/events: HTTP 400 project_id is required...
The server has no project for this event. Fix: set LIGHTTRACK_PROJECT=<your-project-id> ...
```

The unattributed-project case is reported **before** the network call, so it surfaces on the very
first `track*`. Warnings are rate-limited to **one line per error kind per 60 s** (a repeat reports
how many were suppressed), so a hot loop of failing calls costs one line, not thousands.

To turn them off entirely: `LIGHTTRACK_QUIET=1`, or per client — `LightTrack(quiet=True)` (Python),
`new LightTrack({ quiet: true })` (TypeScript), `Client::from_env().quiet(true)` (Rust).

## Python

```python
# export LIGHTTRACK_PROJECT=demo   (or LIGHTTRACK_KEY=lt_... — a project key pins the project)
from lighttrack import LightTrack

lt = LightTrack(source="my-app")            # env: LIGHTTRACK_URL / _KEY / _PROJECT
# ...or pass it explicitly: LightTrack(project="demo", source="my-app")

resp = openai_client.chat.completions.create(model="gpt-4o", messages=[...])
lt.track_openai(resp, latency_ms=120)       # extracts model + token usage

# or time it automatically:
with lt.span("anthropic", "claude-haiku-4-5") as s:
    resp = anthropic_client.messages.create(...)
    s.set_anthropic(resp)

lt.close()                                   # flush at shutdown (also auto-runs at exit)
```

## TypeScript / JavaScript

```ts
// export LIGHTTRACK_PROJECT=demo   (or LIGHTTRACK_KEY=lt_... — a project key pins the project)
import { LightTrack } from "lighttrack-client";

const lt = new LightTrack({ source: "my-app" });   // ...or { project: "demo", source: "my-app" }

const resp = await openai.chat.completions.create({ model: "gpt-4o", messages: [...] });
lt.trackOpenAI(resp, { latencyMs: 120 });

await lt.flush();                            // await in-flight sends before exit
```

## Rust

```rust
// export LIGHTTRACK_PROJECT=demo   (or LIGHTTRACK_KEY=lt_... — a project key pins the project)
use lighttrack_client::{Client, Provider};

let lt = Client::from_env().source("my-app");
// ...or explicitly: Client::new("http://127.0.0.1:8787", None, Some("demo".into()))

lt.event(Provider::OpenAi, "gpt-4o")
    .input_tokens(120).output_tokens(45).latency_ms(120)
    .send();

// or from a provider response JSON value:
lt.track_openai_json(&resp_json, None);

lt.flush();                                  // drain the background worker before exit
```

## Provider field mapping

Each SDK extracts model + token usage from the native response object:

| Provider  | model            | input tokens                         | output tokens                          | cached |
|-----------|------------------|--------------------------------------|----------------------------------------|--------|
| OpenAI    | `model`          | `usage.prompt_tokens` / `input_tokens` | `usage.completion_tokens` / `output_tokens` | `usage.prompt_tokens_details.cached_tokens` / `input_tokens_details.cached_tokens` (Responses API) |
| Anthropic | `model`          | `usage.input_tokens`                 | `usage.output_tokens`                  | `usage.cache_read_input_tokens` (not `cache_creation_…`) |
| Gemini    | `modelVersion` / `model_version` | `usageMetadata.promptTokenCount` / `usage_metadata.prompt_token_count` | `…candidatesTokenCount` / `…candidates_token_count` | `…cachedContentTokenCount` / `…cached_content_token_count` |

Both casings, everywhere they occur: the REST/JS shapes are camelCase and the google-genai Python
objects are snake_case for the same fields. A missing token count is `0`; a *cached* count that was
never reported is unknown, not zero. These readings are pinned for all three SDKs by
[`contract/fixtures/extractors.json`](contract/fixtures/extractors.json).

Provider names are normalized to the API's enum (`openai` / `anthropic` / `google`); common aliases
(`claude`, `gemini`, `vertex`, `azure`, …) are mapped for you.

## Design guarantees

- **Non-blocking:** sends happen off the request path (Python: background daemon thread; TS:
  un-awaited `fetch`; Rust: background worker thread). A full queue drops events rather than blocking.
- **Best-effort:** no network error ever reaches your code — a down or slow LightTrack cannot fail,
  block, or crash your app.
- **Visible, not silent:** a swallowed error is still *reported* — one rate-limited, actionable line
  on stderr per error kind (see [above](#why-dont-i-see-my-events-sdk-diagnostics)). Telemetry that
  fails invisibly is worse than no telemetry. `LIGHTTRACK_QUIET=1` opts out.
- **Flush on exit:** call `close()` / `await flush()` / `flush()` to drain before the process exits.
