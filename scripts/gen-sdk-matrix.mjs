#!/usr/bin/env node
/**
 * Render the SDK capability matrix in `clients/README.md` from the three
 * `clients/<lang>/lighttrack.manifest.json` files.
 *
 * Why generate it. The README promised crash-surviving breadcrumbs and one-line auto-instrumentation
 * in generic prose, while the Rust client has neither — no journal, no span type, no wrapper, no
 * relay. A reader comparing the three had no way to tell a gap from a design choice, and the only
 * thing keeping the page honest was somebody remembering to edit it. Now the manifests are the
 * source, this script is the renderer, and CI diff-checks the result: a capability that changes in
 * one language and not the others shows up as a failing job rather than as a stale sentence.
 *
 *   node scripts/gen-sdk-matrix.mjs           # rewrite the table in clients/README.md
 *   node scripts/gen-sdk-matrix.mjs --check   # exit 1 if the file is not what this would write
 */

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const README = join(ROOT, "clients", "README.md");
const SDKS = ["python", "typescript", "rust"];

const BEGIN = "<!-- BEGIN GENERATED: sdk-capability-matrix (scripts/gen-sdk-matrix.mjs) -->";
const END = "<!-- END GENERATED: sdk-capability-matrix -->";

/** Row order is the order a user meets these features, not alphabetical. */
const CAPABILITIES = [
  ["track", "Record one call (`track*`)"],
  ["span", "Time a call (`span` / context manager)"],
  ["instrument", "Wrap the provider SDK (auto-capture)"],
  ["journal", "Crash-surviving breadcrumbs"],
  ["guard", "Inline output guardrails (`guard`)"],
  ["relay", "Relay tasks (cloud -> device)"],
  ["admit", "Pre-spend admission on limits"],
  ["batch", "Client-side batching"],
];

const MARK = {
  supported: "yes",
  not_supported: "no",
  planned: "planned",
};

function loadManifest(sdk) {
  const path = join(ROOT, "clients", sdk, "lighttrack.manifest.json");
  const m = JSON.parse(readFileSync(path, "utf-8"));
  for (const [cap] of CAPABILITIES) {
    const state = m.capabilities?.[cap];
    if (!(state in MARK)) {
      throw new Error(`${path}: capability '${cap}' is '${state}'; expected one of ${Object.keys(MARK).join(", ")}`);
    }
  }
  return m;
}

function render(manifests) {
  const head = `| Capability | ${SDKS.map((s) => manifests[s].sdk).join(" | ")} |`;
  const rule = `|---|${SDKS.map(() => "---").join("|")}|`;
  const rows = CAPABILITIES.map(([cap, label]) => {
    const cells = SDKS.map((s) => MARK[manifests[s].capabilities[cap]]);
    return `| ${label} | ${cells.join(" | ")} |`;
  });

  // Every `no` and every `planned` gets a line saying what is actually missing, in the SDK's own
  // words. A matrix cell alone reads as a verdict; the note is what makes it usable.
  const notes = [];
  for (const [cap, label] of CAPABILITIES) {
    for (const s of SDKS) {
      const m = manifests[s];
      if (m.capabilities[cap] === "supported") continue;
      const note = m.notes?.[cap];
      if (note) notes.push(`- **${m.sdk} / ${label}** (${MARK[m.capabilities[cap]]}): ${note}`);
    }
  }

  return [
    BEGIN,
    "",
    "<!-- Generated from clients/*/lighttrack.manifest.json. Do not edit by hand: run",
    "     `node scripts/gen-sdk-matrix.mjs`. CI fails if this block is stale. -->",
    "",
    head,
    rule,
    ...rows,
    "",
    "Where a cell is not `yes`, the SDK says why:",
    "",
    ...notes,
    "",
    END,
  ].join("\n");
}

function replaceBlock(text, block) {
  const start = text.indexOf(BEGIN);
  const end = text.indexOf(END);
  if (start === -1 || end === -1) {
    throw new Error(`clients/README.md is missing the generated block markers:\n  ${BEGIN}\n  ${END}`);
  }
  return text.slice(0, start) + block + text.slice(end + END.length);
}

const manifests = Object.fromEntries(SDKS.map((s) => [s, loadManifest(s)]));
const current = readFileSync(README, "utf-8");
const next = replaceBlock(current, render(manifests));

if (process.argv.includes("--check")) {
  if (current !== next) {
    console.error(
      "clients/README.md's capability matrix is stale.\n" +
        "The manifests are the source of truth; regenerate with `node scripts/gen-sdk-matrix.mjs`.",
    );
    process.exit(1);
  }
  console.log("clients/README.md capability matrix is current");
} else {
  writeFileSync(README, next, "utf-8");
  console.log(current === next ? "clients/README.md unchanged" : "clients/README.md capability matrix updated");
}
