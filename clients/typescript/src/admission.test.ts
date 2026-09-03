/**
 * The enforcement half of pre-spend admission: what the client does with a refusal.
 *
 * The verdicts themselves are cross-language and live in the contract suite. What is language-local
 * — and what this file pins — is that `enforce` actually stops the call, that it is off unless asked
 * for, and that a blocked call is never recorded as spend.
 */

import { strict as assert } from "node:assert";
import { test } from "node:test";

import { LightTrack, BLOCKED_TAG } from "./index.ts";
import { LightTrackBudgetExceeded } from "./admission.ts";
import { parseLimitView } from "./limits.ts";

/** A client whose cached view says the project is at its cap. */
function atCap(cfg: Parameters<typeof LightTrack.prototype.constructor>[0] = {}): LightTrack {
  const lt = new LightTrack({ journal: false, quiet: true, ...cfg });
  lt.limits.observe(parseLimitView(200, {}, { usage_ratio: 1.0 }));
  return lt;
}

test("admission is off unless the app asks for it", () => {
  // Adding an observability SDK must not change what an app does. The default has to be inert even
  // when the cached view says the project is over budget.
  const lt = atCap();
  assert.equal(lt.admit().ok, false, "the verdict is still available to read");
  assert.doesNotThrow(() => lt.gate("summarize"), "but nothing is enforced");
});

test('enforce "block" refuses the call with a typed error carrying the reason', () => {
  const lt = atCap({ enforce: "block" });
  try {
    lt.gate("summarize");
    assert.fail("gate must throw when the project is at its cap");
  } catch (e) {
    // Typed, because the host app has to tell "your budget said no" (degrade: smaller model, cache,
    // queue) from a provider outage (retry).
    assert.ok(e instanceof LightTrackBudgetExceeded, `wrong error type: ${e}`);
    assert.equal((e as LightTrackBudgetExceeded).reason, "at_cap");
    assert.equal((e as LightTrackBudgetExceeded).name, "LightTrackBudgetExceeded");
  }
});

test('enforce "warn" reports and proceeds', () => {
  const lt = atCap({ enforce: "warn" });
  assert.doesNotThrow(() => lt.gate("summarize"));
});

test("a blocked call is recorded as traffic, never as spend", async () => {
  const seen: any[] = [];
  const realFetch = globalThis.fetch;
  (globalThis as any).fetch = async (_url: string, init: any) => {
    seen.push(JSON.parse(init.body));
    return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
  };
  try {
    const lt = atCap({ enforce: "block", recordBlocked: true, project: "demo" });
    assert.throws(() => lt.gate("summarize"));
    await lt.flush();
    assert.equal(seen.length, 1, "one event for the blocked call");
    const ev = seen[0];
    assert.deepEqual(ev.tags, [BLOCKED_TAG]);
    // Zero usage and no cost: the call was never made, so inventing spend for it would corrupt
    // exactly the cost report the cap exists to protect.
    assert.equal(ev.usage.input, 0);
    assert.equal(ev.usage.output, 0);
    assert.equal(ev.cost_usd, undefined);
    assert.equal(ev.status, "error");
    assert.equal(ev.metadata.lt_admit_reason, "at_cap");
  } finally {
    (globalThis as any).fetch = realFetch;
  }
});

test("an unobserved client always admits", () => {
  // Fail open: "unknown" must never read as "over budget", or installing LightTrack is an outage.
  const lt = new LightTrack({ journal: false, quiet: true, enforce: "block" });
  assert.equal(lt.admit().ok, true);
  assert.doesNotThrow(() => lt.gate("summarize"));
});
