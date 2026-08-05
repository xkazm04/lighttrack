/**
 * A failed send must be visible, bounded, silenceable — and must still never throw.
 *
 * Network-free: `console.warn` is captured, and the client cases stub `fetch` so we assert on the
 * exact line a user would see without needing a server.
 *
 * Run from `clients/typescript`:  `npx tsx --test src/diagnostics.test.ts`
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { LightTrack } from "./index.ts";
import { Diagnostics, noProjectMessage, sendFailureMessage } from "./diagnostics.ts";

/** Run `fn` with `console.warn` captured; returns everything it printed. */
function captureWarn(fn: () => void | Promise<void>): { lines: string[]; done: Promise<void> } {
  const lines: string[] = [];
  const original = console.warn;
  console.warn = (...args: unknown[]) => {
    lines.push(args.join(" "));
  };
  const restore = () => {
    console.warn = original;
  };
  const r = fn();
  if (r instanceof Promise) return { lines, done: r.finally(restore) };
  restore();
  return { lines, done: Promise.resolve() };
}

test("a tight loop of one error kind prints once", () => {
  const d = new Diagnostics({ quiet: false });
  const { lines } = captureWarn(() => {
    for (let i = 0; i < 1000; i++) d.warn("network", "boom");
  });
  assert.equal(d.emitted, 1, "a tight loop must not flood the console");
  assert.equal(d.suppressed, 999);
  assert.equal(lines.length, 1);
});

test("distinct error kinds each get a line", () => {
  const d = new Diagnostics({ quiet: false });
  captureWarn(() => {
    d.warn("network", "a");
    d.warn("http-400", "b");
    d.warn("network", "a");
  });
  assert.equal(d.emitted, 2);
});

test("cooldown expiry re-warns", () => {
  const d = new Diagnostics({ quiet: false, cooldownMs: 0 });
  captureWarn(() => {
    d.warn("network", "boom");
    d.warn("network", "boom");
  });
  assert.equal(d.emitted, 2);
});

test("quiet silences everything", () => {
  const d = new Diagnostics({ quiet: true });
  const { lines } = captureWarn(() => d.warn("network", "boom"));
  assert.equal(lines.length, 0);
  assert.equal(d.emitted, 0);
});

test("the no-project message names the env var and the constructor arg", () => {
  const m = noProjectMessage("http://127.0.0.1:8787");
  assert.match(m, /LIGHTTRACK_PROJECT/);
  assert.match(m, /project:/);
  assert.match(m, /LIGHTTRACK_KEY/);
});

test("HTTP 400 without a project points at the project setting", () => {
  const m = sendFailureMessage("http://h", "/v1/events", "HTTP 400 project_id is required", {
    status: 400,
    hasProject: false,
    hasKey: true,
  });
  assert.match(m, /LIGHTTRACK_PROJECT/);
  assert.match(m, /project_id is required/);
});

test("an unreachable server points at the url setting", () => {
  assert.match(sendFailureMessage("http://127.0.0.1:1", "/v1/events", "TypeError"), /LIGHTTRACK_URL/);
});

test("messages are ASCII only", () => {
  // They land in whatever console the host app has; a cp1252 Windows terminal mangles anything else.
  for (const m of [
    noProjectMessage("http://h"),
    sendFailureMessage("http://h", "/v1/events", "x", { status: 429 }),
    sendFailureMessage("http://h", "/v1/events", "x", { status: 500 }),
  ]) {
    assert.ok(/^[\x20-\x7e]*$/.test(m), `non-ASCII in: ${m}`);
  }
});

test("an unconfigured client warns and does not throw", async () => {
  const lt = new LightTrack({ baseUrl: "http://127.0.0.1:1", project: undefined, apiKey: undefined });
  const { lines, done } = captureWarn(async () => {
    lt.track("openai", "gpt-4o", { inputTokens: 1 });
    await lt.flush();
  });
  await done;
  assert.ok(
    lines.some((l) => l.includes("LIGHTTRACK_PROJECT")),
    `expected a project warning, got: ${JSON.stringify(lines)}`,
  );
});

test("a non-2xx response is reported, not swallowed", async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async () =>
    new Response('{"error":"project_id is required"}', { status: 400 })) as typeof fetch;
  const lt = new LightTrack({ baseUrl: "http://h", apiKey: "lt_admin_fake" });
  const { lines, done } = captureWarn(async () => {
    lt.track("openai", "gpt-4o", { inputTokens: 1 });
    await lt.flush();
  });
  await done;
  globalThis.fetch = realFetch;
  assert.ok(
    lines.some((l) => l.includes("HTTP 400") && l.includes("LIGHTTRACK_PROJECT")),
    `expected an actionable 400 warning, got: ${JSON.stringify(lines)}`,
  );
});

test("a quiet client says nothing", async () => {
  const lt = new LightTrack({ baseUrl: "http://127.0.0.1:1", quiet: true });
  const { lines, done } = captureWarn(async () => {
    lt.track("openai", "gpt-4o", { inputTokens: 1 });
    await lt.flush();
  });
  await done;
  assert.deepEqual(lines, []);
});
