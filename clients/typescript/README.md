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

## Configure

**Where do my events land?** Every event is attributed to a project. A **project key** pins it
server-side; otherwise the event has to name one. With neither, a **dev-mode** server files events
under a `default` project — fine for a first run — while a server with **authentication enabled**
rejects them. Set one of these as soon as you want events somewhere specific:

```bash
export LIGHTTRACK_URL=http://127.0.0.1:8787   # default; override for a remote server
export LIGHTTRACK_PROJECT=demo                # choose the project (also needed with an admin key)
# ...or instead: export LIGHTTRACK_KEY=lt_...  # a project key pins the project server-side
```

## Use

```ts
import { LightTrack } from "lighttrack-client";

const lt = new LightTrack({ source: "my-app" });   // env: LIGHTTRACK_URL / _KEY / _PROJECT
// ...or pass it explicitly, no env needed:
// const lt = new LightTrack({ project: "demo", source: "my-app" });

const resp = await openai.chat.completions.create({ model: "gpt-4o", messages: [...] });
lt.trackOpenAI(resp, { latencyMs: 120 });          // also: trackAnthropic, trackGemini, track(...)

await lt.flush();                                   // await in-flight sends before exit
```

`lt.span(provider, model)` returns a span; call `span.setOpenAI(resp); span.end()` to record latency
automatically. See `example.ts` and the repo's `clients/README.md`.

### Calls that die mid-flight

An event carries usage and an outcome, so it is sent when the call *finishes*. That alone would mean
a process killed mid-call (OOM killer, SIGKILL, a container eviction) leaves no record of a call that
definitely happened and definitely cost money — the exact calls you most want afterwards.

So opening a span also writes a small **crash-surviving breadcrumb**, retired on every exit path
(success and failure alike). The next client constructed with the same journal directory re-reports
anything left unsettled, as `status: "error"` with the reason spelled out and a
`lighttrack:unsettled-span` tag — never as a clean zero-cost success, because nothing ever reported
an outcome. `await lt.recovered` resolves with how many were re-reported; `await lt.close()` drains
sends and retires this process's journal on an orderly exit.

Knobs, and the honest limit: `LIGHTTRACK_JOURNAL=0` (or `new LightTrack({ journal: false })`) turns
it off; `LIGHTTRACK_JOURNAL_DIR` chooses where breadcrumbs live; `LIGHTTRACK_JOURNAL_ORPHAN_MS`
(default 300000) is how long another process's journal must sit untouched before it is treated as
abandoned. It is a no-op outside Node — a browser has nowhere to leave a breadcrumb — and recovery
needs a later client to see that directory, so a container rescheduled onto fresh storage is not
covered unless the directory is a mounted volume.

## Why don't I see my events?

`track*` never throws — but it is not silent. A failed send emits one actionable line via
`console.warn` (stderr in Node, never stdout, which your app may be using as a protocol channel):

```
[lighttrack] no project is configured, so these events are not attributed: a dev-mode server files
them under the 'default' project, and a server with authentication enabled rejects them. To choose
where they land, set LIGHTTRACK_PROJECT=<your-project-id> ...
```

That case is reported *before* the request, so it appears on your very first `track*`. Warnings are
rate-limited to one line per error kind per 60 s, so a hot loop costs one line, not thousands.

Silence them with `LIGHTTRACK_QUIET=1` or `new LightTrack({ quiet: true })`.

## Test

```bash
cd clients/typescript && npm install && npm test   # also: npx tsc --noEmit -p tsconfig.json
```

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
