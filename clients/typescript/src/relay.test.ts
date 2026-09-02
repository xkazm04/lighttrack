/**
 * The relay admission verdict as an SDK caller sees it (M18).
 *
 * Relay calls are functional — they throw — but "threw" was all a caller could learn, so an
 * unroutable action type (nothing in the fleet advertises it, and nothing ever will until somebody
 * changes the fleet) was indistinguishable from a timeout worth retrying. `RelayError.code` and
 * `isUnroutable` are what make that a decision the app can take.
 */

import { strict as assert } from "node:assert";
import { test } from "node:test";

import { errorCode, RelayError, type RelayAdmission } from "./index.ts";

test("relay: an unroutable refusal is distinguishable from a retryable failure", () => {
  const body = JSON.stringify({
    error: {
      code: "relay_unroutable",
      message: "no enrolled device advertises 'xprice/typo' (2 device(s) enrolled)",
    },
  });
  const refused = new RelayError(`POST /v1/relay/tasks -> HTTP 422: ${body}`, {
    code: errorCode(body),
    status: 422,
  });
  assert.equal(refused.isUnroutable, true);
  assert.equal(refused.code, "relay_unroutable");
  assert.equal(refused.status, 422);
  // The reason survives into the message, because it names the fix (the spelling, or a device's
  // capabilities) and an SDK that swallowed it would leave the caller with only "422".
  assert.ok(refused.message.includes("xprice/typo"));

  // A transient failure is NOT this: retrying it is the right thing, and `isUnroutable` is what
  // stops an app burning its retry budget on a task that can never run.
  const transient = new RelayError("POST /v1/relay/tasks failed: connect ECONNREFUSED", {});
  assert.equal(transient.isUnroutable, false);
  assert.equal(transient.code, undefined);
  assert.equal(transient.status, undefined);

  const overloaded = new RelayError("HTTP 503", { code: "overloaded", status: 503 });
  assert.equal(overloaded.isUnroutable, false);
});

test("relay: a malformed error body degrades to no code rather than replacing the failure", () => {
  // An error response is exactly when a body is least likely to be well-formed — a proxy's HTML,
  // a truncated stream. Parsing must never turn "the server rejected you" into "invalid JSON".
  assert.equal(errorCode("<html>502 Bad Gateway</html>"), undefined);
  assert.equal(errorCode(""), undefined);
  assert.equal(errorCode("{}"), undefined);
  assert.equal(errorCode(JSON.stringify({ error: "a string, not an object" })), undefined);
  assert.equal(errorCode(JSON.stringify({ error: { code: 422 } })), undefined);
  assert.equal(errorCode(JSON.stringify({ error: { code: "not_found" } })), "not_found");
});

test("relay: an accepted enqueue says how much of the fleet could run it", () => {
  const admission: RelayAdmission = { verdict: "queued", eligible_devices: 2 };
  assert.equal(admission.verdict, "queued");
  assert.equal(admission.eligible_devices, 2);

  // Zero is not a refusal — it means no devices are enrolled at all (the legacy single-device
  // deployment, which routes fine). An app that treated it as one would break on every instance
  // that has not enrolled anything yet.
  const legacy: RelayAdmission = { verdict: "queued", eligible_devices: 0 };
  assert.equal(legacy.verdict, "queued");
});
