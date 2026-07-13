/**
 * Payload-shape tests for auto-instrument trace/span linkage. Network-free: a LightTrack whose
 * `post` is stubbed captures every emitted event, so we assert the `trace_id` / `span_id` /
 * `parent_span_id` shape end-to-end through `wrapOpenAI`. The span tree is only real if the first
 * call in a span *owns* its id and later calls hang under it.
 *
 * Run from `clients/typescript`:  `npx tsx --test src/instrument.test.ts`
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  LightTrack,
  wrapOpenAI,
  withTrace,
  withSpan,
  currentSpanId,
  currentTraceId,
} from "./index.ts";

/** A wrapped fake OpenAI client whose emitted events are captured (no network). */
function harness() {
  const calls: any[] = [];
  const lt = new LightTrack();
  (lt as any).post = (_path: string, body: any) => {
    calls.push(body);
  };
  const client: any = {
    chat: {
      completions: {
        create: async () => ({ model: "gpt-4o", usage: { prompt_tokens: 1, completion_tokens: 1 } }),
      },
    },
  };
  wrapOpenAI(client, lt);
  const call = () => client.chat.completions.create({ model: "gpt-4o" });
  return { calls, call };
}

test("call without a span is a standalone root", async () => {
  const { calls, call } = harness();
  await call();
  assert.ok(calls[0].span_id);
  assert.equal(calls[0].parent_span_id, undefined);
  assert.ok(calls[0].trace_id);
});

test("first call owns the span; later calls nest under it", async () => {
  const { calls, call } = harness();
  await withSpan(
    async () => {
      await call();
      await call();
      await call();
    },
    { spanId: "S1" },
  );
  assert.equal(calls[0].span_id, "S1");
  assert.equal(calls[0].parent_span_id, undefined);
  assert.notEqual(calls[1].span_id, "S1");
  assert.equal(calls[1].parent_span_id, "S1");
  assert.equal(calls[2].parent_span_id, "S1");
  assert.equal(new Set(calls.map((c) => c.trace_id)).size, 1);
});

test("nested spans chain to their enclosing span", async () => {
  const { calls, call } = harness();
  await withTrace(
    async () => {
      await withSpan(
        async () => {
          await call(); // owns S1
          await withSpan(
            async () => {
              await call(); // owns S2, parent S1
              await call(); // child of S2
            },
            { spanId: "S2" },
          );
          await call(); // child of S1
        },
        { spanId: "S1" },
      );
    },
    { traceId: "T" },
  );
  const [s1o, s2o, s2c, s1c] = calls;
  assert.equal(s1o.span_id, "S1");
  assert.equal(s1o.parent_span_id, undefined);
  assert.equal(s2o.span_id, "S2");
  assert.equal(s2o.parent_span_id, "S1"); // inner span chains to the outer
  assert.equal(s2c.parent_span_id, "S2");
  assert.equal(s1c.parent_span_id, "S1");
  assert.ok(calls.every((c) => c.trace_id === "T"));
});

test("context is restored after the blocks exit", async () => {
  await withTrace(
    async () => {
      await withSpan(
        async () => {
          assert.equal(currentSpanId(), "S1");
        },
        { spanId: "S1" },
      );
      assert.equal(currentSpanId(), undefined);
      assert.equal(currentTraceId(), "T");
    },
    { traceId: "T" },
  );
  assert.equal(currentTraceId(), undefined);
});
