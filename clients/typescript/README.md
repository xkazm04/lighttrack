# lighttrack-client (TypeScript / JavaScript)

Fire-and-forget client for ingesting LLM call events into [LightTrack](https://github.com/xkazm04/tracklight).
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
