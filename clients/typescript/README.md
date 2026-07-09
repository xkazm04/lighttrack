# lighttrack-client (TypeScript / JavaScript)

Fire-and-forget client for ingesting LLM call events into [LightTrack](https://github.com/xkazm04/lighttrack).
Uses the global `fetch` (Node 18+ / browsers); zero runtime dependencies. `track*` never blocks and
never throws.

## Install / build

```bash
cd clients/typescript
npm install      # dev: typescript only
npm run build    # emits dist/ (ESM + types)
```

You can also vendor `src/index.ts` directly. Node 22.18+/23+/24 runs the `.ts` sources without a build
step (type stripping): `node example.ts`.

## Use

```ts
import { LightTrack } from "lighttrack-client";

const lt = new LightTrack({ source: "my-app" });   // env: LIGHTTRACK_URL / _KEY / _PROJECT

const resp = await openai.chat.completions.create({ model: "gpt-4o", messages: [...] });
lt.trackOpenAI(resp, { latencyMs: 120 });          // also: trackAnthropic, trackGemini, track(...)

await lt.flush();                                   // await in-flight sends before exit
```

`lt.span(provider, model)` returns a span; call `span.setOpenAI(resp); span.end()` to record latency
automatically. See `example.ts` and the repo's `clients/README.md`.

## Auto-instrument (one line)

Skip the per-call `track*`. Wrap a provider SDK client once and every call is captured automatically
(model, usage, latency, trace ids). `withTrace` shares a `trace_id`; `withSpan` nests `parent_span_id`:

```ts
import { wrapOpenAI, wrapAnthropic, wrapGemini, withTrace, withSpan } from "lighttrack-client";

const openai = wrapOpenAI(new OpenAI());   // drop-in: same client object back, now observed
await withTrace(async () => {
  await openai.chat.completions.create({ ... });   // auto-tracked (trace root)
  await withSpan(async () => {
    await openai.chat.completions.create({ ... });  // auto-tracked (child span)
  });
});
```

`wrap(client)` auto-detects which of the three SDKs it is. Best-effort: instrumentation never throws
into your app. Trace context is a per-process global swapped around the awaited callback — for
strictly concurrent, interleaved traces, pass an explicit `traceId` to keep them isolated.

## Relay tasks (offline device work)

Enqueue heavy, offline-tolerant LLM tasks for the enrolled local device running `lt-agent`
(executed via Claude Code on subscription; see `docs/RELAY.md`). Unlike `track*` telemetry these
are functional calls: they resolve with the task and throw `RelayError` on failure.

```ts
const task = await lt.relayTask("xprice/reprice-summary", {
  payload: { sku: "A-1" },
  idempotencyKey: "order-42",
});
const done = await lt.waitRelayTask(task.id);   // optional poll; prefer the connector push
if (done.status === "succeeded") console.log(done.result);
```
