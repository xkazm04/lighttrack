/**
 * The cross-language SDK contract, run against the TypeScript client.
 *
 * Every case here also runs, unchanged, in `clients/python/tests/test_contract.py` and
 * `clients/rust/tests/contract.rs`. That is the whole point: the three SDKs were three
 * hand-synchronised implementations of one contract, and nothing could see the drift between them —
 * the provider extractors were triplicated, the PII table was triplicated and one of the three had
 * gone stale against the server, and CI ran the suites as unrelated jobs. Shared vectors turn "we
 * believe these agree" into a test.
 *
 * A behaviour that is not in `clients/contract/fixtures/` is not part of the contract, and a
 * behaviour that is may not differ between languages. Capabilities a given SDK does not have are
 * declared `not_supported` in its `lighttrack.manifest.json` and skipped here, honestly and visibly,
 * rather than quietly not asserted.
 */

import { strict as assert } from "node:assert";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { diagnosticKind, sendFailureMessage } from "./diagnostics.ts";
import { extractAnthropic, extractGemini, extractOpenAI, guard, type GuardRules } from "./index.ts";
import { parseLimitView } from "./limits.ts";
import { unsettled } from "./journal.ts";
import { PII_RULES, type PiiRule } from "./pii.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(HERE, "..", "..", "contract", "fixtures");
const MANIFEST = join(HERE, "..", "lighttrack.manifest.json");
const PII_MODULE = join(HERE, "pii.ts");

function fixture(name: string): any {
  return JSON.parse(readFileSync(join(FIXTURES, `${name}.json`), "utf-8"));
}

const manifest = JSON.parse(readFileSync(MANIFEST, "utf-8"));
const supports = (cap: string) => manifest.capabilities?.[cap] === "supported";

// ---- pii.ts is generated from the fixture -----------------------------------
//
// The fixture is exported from `crates/anon` (the server's own scrubber). A bundler-safe SDK cannot
// read a JSON file outside its package at runtime — it has to ship in the browser — so the table is
// emitted as a TypeScript module and this test is what keeps it honest.
// Regenerate: `LIGHTTRACK_UPDATE_FIXTURES=1 npm test`.

function renderPiiModule(rules: PiiRule[]): string {
  const rows = rules
    .map((r) => `  { kind: ${JSON.stringify(r.kind)}, pattern: ${JSON.stringify(r.pattern)}, placeholder: ${JSON.stringify(r.placeholder)} },`)
    .join("\n");
  return `/**
 * GENERATED FILE - do not edit.
 *
 * The PII rule set the LightTrack server scrubs ingest with, exported by \`crates/anon\` to
 * \`clients/contract/fixtures/pii.json\` and rendered here so \`guard({ noPII: true })\` runs exactly
 * the rules the ingest path runs. Before this file the SDK carried its own four-row copy, which had
 * drifted: it still ran the pre-D14 phone regex that flags every ISO date as a phone number.
 *
 * Rules are in evaluation order (most specific first) and several may share a \`kind\`.
 *
 * Regenerate with \`LIGHTTRACK_UPDATE_FIXTURES=1 npm test\` after changing crates/anon.
 */

export interface PiiRule {
  /** Family name: email, iban, ssn, secret, phone, credit_card, ip. */
  kind: string;
  /** Restricted to the RE2 / JS / Python / Rust common subset: no lookaround, no backreferences. */
  pattern: string;
  placeholder: string;
}

export const PII_RULES: readonly PiiRule[] = [
${rows}
];
`;
}

test("contract: the PII table is the server's, not a copy", () => {
  const rules: PiiRule[] = fixture("pii").rules;
  const rendered = renderPiiModule(rules);
  if (process.env.LIGHTTRACK_UPDATE_FIXTURES) {
    writeFileSync(PII_MODULE, rendered, "utf-8");
    return;
  }
  assert.equal(
    readFileSync(PII_MODULE, "utf-8"),
    rendered,
    "src/pii.ts has drifted from clients/contract/fixtures/pii.json. The server's scrubber is the " +
      "source of truth; regenerate with `LIGHTTRACK_UPDATE_FIXTURES=1 npm test`.",
  );
  assert.deepEqual([...PII_RULES], rules, "the compiled-in table must equal the fixture");
});

test("contract: every PII pattern compiles in JavaScript", () => {
  for (const r of PII_RULES) {
    assert.doesNotThrow(() => new RegExp(r.pattern), `rule '${r.kind}' does not compile: ${r.pattern}`);
  }
});

// ---- extractors -------------------------------------------------------------

test("contract: provider extractors", () => {
  for (const c of fixture("extractors").extractors) {
    const [model, input, output, cached] =
      c.provider === "openai"
        ? extractOpenAI(c.response)
        : c.provider === "anthropic"
          ? extractAnthropic(c.response)
          : extractGemini(c.response);
    const got = {
      model: model ?? null,
      input_tokens: input,
      output_tokens: output,
      cached_input_tokens: cached ?? null,
    };
    assert.deepEqual(got, c.expect, `${c.name}: ${c.why ?? ""}`);
  }
});

// ---- guard ------------------------------------------------------------------

/** Map the fixture's neutral snake_case rules onto this SDK's camelCase public shape. */
function toRules(r: Record<string, unknown>): GuardRules {
  return {
    json: r.json as boolean | undefined,
    jsonKeys: r.json_keys as string[] | undefined,
    maxWords: r.max_words as number | undefined,
    minWords: r.min_words as number | undefined,
    maxChars: r.max_chars as number | undefined,
    mustInclude: r.must_include as string[] | undefined,
    mustMatch: r.must_match as string | undefined,
    mustNotMatch: r.must_not_match as string[] | undefined,
    noPII: r.no_pii as boolean | undefined,
  };
}

test("contract: guard verdicts", () => {
  for (const c of fixture("guard").guard) {
    const result = guard(c.output, toRules(c.rules));
    const failed = Object.entries(result.checks)
      .filter(([, passed]) => !passed)
      .map(([k]) => k)
      .sort();
    assert.deepEqual(failed, [...c.expect.violations].sort(), `${c.name}: ${c.why ?? ""}`);
    assert.equal(result.ok, c.expect.ok, `${c.name}: ok`);
    // `ok` is defined as "nothing failed" — the two must never disagree.
    assert.equal(result.ok, result.violations.length === 0, `${c.name}: ok tracks violations`);
  }
});

// ---- journal ----------------------------------------------------------------

test("contract: journal unsettled records", { skip: !supports("journal") }, () => {
  for (const c of fixture("journal").journal) {
    assert.deepEqual(unsettled(c.body), c.expect, `${c.name}: ${c.why ?? ""}`);
  }
});

// ---- limits -----------------------------------------------------------------

test("contract: ingest limit signals", () => {
  for (const c of fixture("limits").limits) {
    const v = parseLimitView(c.status, c.headers, c.body);
    assert.deepEqual(
      {
        accepted: v.accepted,
        rate_limited: v.rateLimited,
        usage_ratio: v.usageRatio,
        shed_fraction: v.shedFraction,
        retry_after_secs: v.retryAfterSecs,
        error_code: v.errorCode,
        binding_scope: v.bindingScope,
        binding_rule: v.bindingRule,
      },
      c.expect,
      `${c.name}: ${c.why ?? ""}`,
    );
  }
});

// ---- diagnostics ------------------------------------------------------------

test("contract: failure diagnostics", () => {
  for (const c of fixture("diagnostics").diagnostics) {
    const status = c.status ?? undefined;
    assert.equal(diagnosticKind(status), c.kind, `${c.name}: rate-limiting bucket`);
    const msg = sendFailureMessage("http://127.0.0.1:8787", "/v1/events", "boom", {
      status,
      hasProject: c.has_project,
      hasKey: c.has_key,
    });
    for (const needle of c.hint_contains) {
      assert.ok(msg.includes(needle), `${c.name}: message is missing "${needle}".\nGot: ${msg}`);
    }
    // ASCII only. These lines land in whatever console the host app has, and a cp1252 Windows
    // terminal turns a stray em dash into mojibake.
    assert.ok(/^[\x20-\x7e\s]*$/.test(msg), `${c.name}: message must be ASCII.\nGot: ${msg}`);
  }
});
