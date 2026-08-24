/**
 * The crash-surviving span journal: what a killed process leaves behind, and what a later one does
 * with it.
 *
 * The defect under test is the one that mattered most in an observability client: `Span` emitted its
 * event only in `end()`, so a process killed mid-call left no record of a call that definitely
 * happened and definitely cost money. These tests pin the mechanism that closes it, and —
 * deliberately — also pin the ways it must NOT lie: a settled call leaves nothing behind, a live
 * process's journal is not stolen, and a recovered call reads as unknown-outcome rather than as a
 * zero-cost success.
 */
import { strict as assert } from "node:assert";
import { mkdtempSync, readdirSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { LightTrack } from "./index.ts";
import { RECOVERED_TAG, SpanJournal, unsettled, unsettledError } from "./journal.ts";

/**
 * Capture what the client would send by stubbing the global `fetch` — the real send path, minus the
 * network. Stubbing the transport rather than subclassing keeps the test honest about `post`'s
 * private, best-effort contract and keeps `tsc` (a blocking CI step) happy about it.
 */
const sent: Array<Record<string, any>> = [];
const realFetch = globalThis.fetch;
globalThis.fetch = (async (_url: any, init: any) => {
  sent.push(JSON.parse(String(init?.body ?? "{}")));
  return new Response("", { status: 200 });
}) as typeof fetch;
process.on("exit", () => {
  globalThis.fetch = realFetch;
});

function newDir(): string {
  sent.length = 0;
  const d = mkdtempSync(join(tmpdir(), "lt-journal-"));
  return d.split("\\").join("/");
}

function client(dir: string): LightTrack {
  return new LightTrack({ baseUrl: "http://127.0.0.1:1", project: "p", quiet: true, journalDir: dir });
}

/** Backdate every journal file so the freshness heuristic treats it as abandoned. */
function age(dir: string, seconds = 10_000): void {
  const t = new Date(Date.now() - seconds * 1000);
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isFile()) utimesSync(p, t, t);
  }
}

/** The journal is async to start (it imports node:fs); give it a tick before asserting on disk. */
const settled = () => new Promise((r) => setTimeout(r, 25));

test("a span that never ends is recovered by the next client", async () => {
  const dir = newDir();
  const crashed = client(dir);
  crashed.span("openai", "gpt-4o", { name: "summarize" }); // ...and the process is killed here
  await settled();
  assert.equal(sent.length, 0, "nothing is emitted while the call is in flight");
  age(dir);

  const survivor = client(dir);
  await survivor.recovered;
  await survivor.flush();
  assert.equal(sent.length, 1, `the killed call must survive: ${JSON.stringify(sent)}`);
  const ev = sent[0];
  assert.equal(ev.provider, "openai");
  assert.equal(ev.model, "gpt-4o");
  assert.equal(ev.name, "summarize");
  assert.ok(ev.tags.includes(RECOVERED_TAG));
  rmSync(dir, { recursive: true, force: true });
});

test("a recovered call reports an unknown outcome, not a success", async () => {
  const dir = newDir();
  client(dir).span("anthropic", "claude");
  await settled();
  age(dir);

  const after = client(dir);
  await after.recovered;
  await after.flush();
  const ev = sent[0];
  assert.equal(ev.status, "error");
  assert.match(ev.error, /unsettled span/);
  assert.equal(ev.latency_ms, undefined, "latency is unknown, not the time until it was noticed");
  assert.match(ev.error, /unknown, not zero/, "the zeros must be labelled as unknown");
  rmSync(dir, { recursive: true, force: true });
});

test("a settled span leaves nothing to recover", async () => {
  const dir = newDir();
  const lt = client(dir);
  const s = lt.span("openai", "gpt-4o");
  s.setUsage(10, 20);
  s.end();
  await lt.flush();
  await settled();
  sent.length = 0; // the settled call's own event is not what this test is about
  age(dir);

  const after = client(dir);
  await after.recovered;
  await after.flush();
  assert.deepEqual(sent, [], "a call whose outcome WAS observed must not be re-reported");
  rmSync(dir, { recursive: true, force: true });
});

test("a failed span is settled too", async () => {
  // The error path is an exit path. If it did not retire the breadcrumb, every failed call would
  // later resurface a second time as a phantom unsettled one.
  const dir = newDir();
  const lt = client(dir);
  lt.span("openai", "gpt-4o").end(new Error("boom"));
  await lt.flush();
  await settled();
  sent.length = 0; // the failed call's own event is not what this test is about
  age(dir);

  const after = client(dir);
  await after.recovered;
  await after.flush();
  assert.deepEqual(sent, []);
  rmSync(dir, { recursive: true, force: true });
});

test("a recent journal is left alone", async () => {
  // A second process starting up must not steal a live one's in-flight calls.
  const dir = newDir();
  client(dir).span("openai", "gpt-4o");
  await settled();
  // No ageing: the file was written moments ago.
  const other = client(dir);
  await other.recovered;
  await other.flush();
  assert.deepEqual(sent, []);
  rmSync(dir, { recursive: true, force: true });
});

test("recovery happens once", async () => {
  const dir = newDir();
  client(dir).span("openai", "gpt-4o");
  await settled();
  age(dir);

  const first = client(dir);
  assert.equal(await first.recovered, 1);
  age(dir);
  const second = client(dir);
  assert.equal(await second.recovered, 0, "a breadcrumb is consumed, not replayed forever");
  rmSync(dir, { recursive: true, force: true });
});

test("a torn final line does not lose the records before it", () => {
  // A kill mid-write leaves a partial last line. One record per line exists precisely so that the
  // torn one is the only casualty.
  const text = [
    '{"o":"b","k":1,"m":"a"}',
    '{"o":"b","k":2,"m":"b"}',
    '{"o":"e","k":1}',
    '{"o":"b","k":3,"m":"ope', // killed mid-write
  ].join("\n");
  assert.deepEqual(
    unsettled(text).map((r) => r.m),
    ["b"],
  );
});

test("a disabled journal writes nothing", async () => {
  const dir = newDir();
  const j = new SpanJournal({ enabled: false, dir });
  assert.equal(j.begin({ p: "openai" }), undefined);
  j.settle(undefined);
  await settled();
  assert.deepEqual(readdirSync(dir), []);
  rmSync(dir, { recursive: true, force: true });
});

test("an unusable directory never breaks the caller", async () => {
  const dir = newDir();
  const blocker = join(dir, "f");
  writeFileSync(blocker, "");
  const j = new SpanJournal({ dir: `${dir}/f/not-a-dir` });
  await settled();
  j.begin({ p: "openai" }); // must not throw
  assert.deepEqual(await j.recover(), []);
  rmSync(dir, { recursive: true, force: true });
});

test("the unsettled reason names what is known and what is not", () => {
  const msg = unsettledError({ t: 1_700_000_000_000 });
  assert.match(msg, /2023-11-14/);
  assert.match(msg, /exited or stalled/);
});
