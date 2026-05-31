# LightTrack client SDKs

Thin, **fire-and-forget** libraries that send your apps' LLM calls to a LightTrack server. They wrap
an OpenAI / Anthropic / Gemini result, normalize it, and POST it to `/v1/events` — without blocking
your request path and without ever throwing into your app (telemetry must never break the caller).

The API fills in the rest: it derives the **project from the API key**, assigns the event id and
timestamp, and computes **cost** from its price book. So the minimal event is just
`{provider, model, usage}`.

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
- `LIGHTTRACK_PROJECT` — project id to stamp on events. Needed in **dev mode** (no key) or when using
  an **admin key** to ingest into a specific project; ignored when a project key already pins it.

## Python

```python
from lighttrack import LightTrack

lt = LightTrack(source="my-app")            # env: LIGHTTRACK_URL / _KEY / _PROJECT

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
import { LightTrack } from "lighttrack-client";

const lt = new LightTrack({ source: "my-app" });

const resp = await openai.chat.completions.create({ model: "gpt-4o", messages: [...] });
lt.trackOpenAI(resp, { latencyMs: 120 });

await lt.flush();                            // await in-flight sends before exit
```

## Rust

```rust
use lighttrack_client::{Client, Provider};

let lt = Client::from_env().source("my-app");

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
| OpenAI    | `model`          | `usage.prompt_tokens` / `input_tokens` | `usage.completion_tokens` / `output_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| Anthropic | `model`          | `usage.input_tokens`                 | `usage.output_tokens`                  | `usage.cache_read_input_tokens` |
| Gemini    | `model_version`  | `usageMetadata.promptTokenCount`     | `usageMetadata.candidatesTokenCount`   | `usageMetadata.cachedContentTokenCount` |

Provider names are normalized to the API's enum (`openai` / `anthropic` / `google`); common aliases
(`claude`, `gemini`, `vertex`, `azure`, …) are mapped for you.

## Design guarantees

- **Non-blocking:** sends happen off the request path (Python: background daemon thread; TS:
  un-awaited `fetch`; Rust: background worker thread). A full queue drops events rather than blocking.
- **Best-effort:** all network errors are swallowed; a down or slow LightTrack never affects your app.
- **Flush on exit:** call `close()` / `await flush()` / `flush()` to drain before the process exits.
