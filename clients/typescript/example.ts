/**
 * Runnable example for the LightTrack TypeScript client.
 *
 * Node 22.18+/23+/24 runs this `.ts` file directly (type stripping). Start the API first, then:
 *
 *     node example.ts                              # dev mode: ingests into project "demo"
 *     LIGHTTRACK_KEY=lt_... node example.ts        # enforced mode: project from the key
 *
 * Uses fake provider response objects, so it runs with no real provider SDK or API key.
 */

import { LightTrack } from "./src/index.ts";

const fakeOpenAI = {
  model: "gpt-4o-mini",
  usage: { prompt_tokens: 120, completion_tokens: 45, prompt_tokens_details: { cached_tokens: 64 } },
};
const fakeAnthropic = {
  model: "claude-haiku-4-5",
  usage: { input_tokens: 200, output_tokens: 80, cache_read_input_tokens: 0 },
};
const fakeGemini = {
  modelVersion: "gemini-2.5-flash",
  usageMetadata: { promptTokenCount: 90, candidatesTokenCount: 30 },
};

async function main() {
  const project = process.env.LIGHTTRACK_KEY ? undefined : process.env.LIGHTTRACK_PROJECT ?? "demo";
  const lt = new LightTrack({ project, source: "example.ts", tags: ["demo"] });

  lt.trackOpenAI(fakeOpenAI, { latencyMs: 210, traceId: "t-1" });
  lt.trackAnthropic(fakeAnthropic, { latencyMs: 540 });
  lt.trackGemini(fakeGemini, undefined, { latencyMs: 300 });

  // Timing span: latency measured automatically; usage pulled from the response.
  const span = lt.span("openai", undefined, { tags: ["span"] });
  span.setOpenAI(fakeOpenAI);
  span.end();

  lt.track("openai", "gpt-4o", { inputTokens: 10, outputTokens: 5, operation: "chat" });

  await lt.flush();
  console.log("sent 5 events — check: GET /v1/events and /v1/costs");
}

main();
