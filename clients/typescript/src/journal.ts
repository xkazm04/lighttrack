/**
 * Crash-surviving breadcrumbs for calls that are still in flight.
 *
 * The defect this closes (identical to the Python client's): `Span` emitted its event only in
 * `end()`, so the coverage profile was exactly inverted. Orderly completions were recorded
 * perfectly; a process killed mid-call — OOM killer, SIGKILL, a container eviction — left **no
 * record at all** of a call that definitely happened and definitely cost money. For an
 * observability product that silently drops exactly the calls an operator most needs.
 *
 * The rule: durability must not be conditioned on the run ending the way the writer expects. A span
 * becomes durable when it OPENS, not when it closes.
 *
 * Why a local journal rather than a span-open POST. The server's ingest is an insert keyed on event
 * id (a duplicate is a 409 by design), and the SQLite backend's rolling-usage cache for limit
 * admission folds committed rows by rowid and cannot observe an in-place update — so a
 * settle-by-update would make spend caps silently wrong. A local append-only journal buys the same
 * crash coverage with no server change and no accounting risk. Its honest limit, written down
 * rather than glossed: recovery happens when a LightTrack client next starts **with the same
 * journal directory**. A container that dies and is rescheduled onto fresh storage is not covered.
 *
 * Durability level: each record is appended with `appendFileSync`, which returns after the write
 * reaches the OS. That survives the *process* dying, which is the case in scope. It does not
 * `fsync` — paying a disk round trip per span boundary would put latency on the call path this
 * client promises never to block.
 *
 * Outside Node (browser, edge runtime, any host with no `node:fs`) every method is a silent no-op:
 * there is nowhere to leave a breadcrumb, and a client that threw there would be worse than one
 * that records a little less.
 */

const FILE_PREFIX = "lighttrack-spans-";
const FILE_SUFFIX = ".jsonl";

/**
 * How long ANOTHER process's journal must have been untouched before this one treats its open
 * records as orphaned. A liveness heuristic, chosen over a pid probe on purpose: pid liveness is not
 * portable and pids are reused, so a stale journal could be judged live by a brand-new unrelated
 * process. A live client touches its journal on every span boundary, so a busy process is never
 * mistaken for a dead one. The exposure is a single call open longer than this window — reported as
 * unsettled while in fact still running, which is honest either way (a call in flight for five
 * minutes is a fact an operator wants).
 */
export const DEFAULT_ORPHAN_MS = 300_000;

/** Tag on every event reconstructed from a journal — filterable, never silently mixed with calls
 * whose outcome was actually observed. */
export const RECOVERED_TAG = "lighttrack:unsettled-span";

/** What was known when a call started. Field names are short because this is written per call. */
export interface JournalRecord {
  /** provider */ p?: string;
  /** model */ m?: string;
  /** name */ n?: string;
  /** operation */ op?: string;
  /** trace id */ tr?: string;
  /** span id */ sp?: string;
  /** parent span id */ ps?: string;
  /** project */ pj?: string;
  /** started at, epoch ms */ t?: number;
}

type NodeFs = typeof import("node:fs");

let fsMod: NodeFs | null | undefined;
let fsLoading: Promise<void> | undefined;

/** Load `node:fs` once, tolerating hosts that do not have it. */
function ensureFs(): Promise<void> {
  if (fsLoading) return fsLoading;
  fsLoading = import("node:fs")
    .then((m) => {
      fsMod = m;
    })
    .catch(() => {
      fsMod = null; // not Node — the journal is a no-op here, by design
    });
  return fsLoading;
}

function env(name: string): string | undefined {
  return typeof process !== "undefined" && process.env ? process.env[name] : undefined;
}

function enabledFromEnv(): boolean {
  const v = (env("LIGHTTRACK_JOURNAL") ?? "").trim().toLowerCase();
  return !["0", "false", "no", "off"].includes(v);
}

function defaultDir(): string {
  const explicit = env("LIGHTTRACK_JOURNAL_DIR");
  if (explicit) return explicit;
  const tmp = env("TMPDIR") ?? env("TEMP") ?? env("TMP") ?? "/tmp";
  return `${tmp.replace(/[\\/]+$/, "")}/lighttrack-spans`;
}

export interface SpanJournalOptions {
  enabled?: boolean;
  dir?: string;
  orphanAfterMs?: number;
}

export class SpanJournal {
  readonly enabled: boolean;
  readonly dir: string;
  readonly orphanAfterMs: number;
  /** The file this instance owns. `undefined` until the first `begin()` — an idle client leaves
   * nothing on disk. */
  path?: string;
  private nextKey = 0;
  private openKeys = new Set<number>();
  private broken = false;
  private pending: string[] = [];

  constructor(opts: SpanJournalOptions = {}) {
    this.enabled = opts.enabled ?? enabledFromEnv();
    this.dir = opts.dir ?? defaultDir();
    this.orphanAfterMs = opts.orphanAfterMs ?? Number(env("LIGHTTRACK_JOURNAL_ORPHAN_MS") ?? DEFAULT_ORPHAN_MS);
    if (this.enabled) void ensureFs();
  }

  /** Record that a call has STARTED. Returns a token for `settle`, or undefined when there is
   * nothing to settle (journal off, no filesystem, or already degraded). */
  begin(rec: JournalRecord): number | undefined {
    if (!this.enabled || this.broken) return undefined;
    const key = ++this.nextKey;
    this.openKeys.add(key);
    this.write(JSON.stringify({ ...rec, t: rec.t ?? Date.now(), o: "b", k: key }));
    return key;
  }

  /** Retire the breadcrumb. The event itself goes through the normal path; this only says the
   * outcome was observed, so nothing needs reconstructing. */
  settle(key: number | undefined): void {
    if (key === undefined || !this.enabled || this.broken) return;
    this.openKeys.delete(key);
    this.write(JSON.stringify({ o: "e", k: key }));
    // Nothing in flight ⇒ nothing to recover ⇒ back to empty. This is what keeps a long-lived
    // process's journal from growing without bound.
    if (this.openKeys.size === 0) this.truncate();
  }

  /** Orderly shutdown: a journal with nothing in flight is deleted, so an orphan sweep finds only
   * real orphans. The stale window is the price of *crash* detection; paying it on a clean exit is
   * pure waste. */
  close(): void {
    if (!fsMod || !this.path) return;
    try {
      if (this.openKeys.size === 0) fsMod.rmSync(this.path, { force: true });
    } catch {
      /* best effort */
    }
  }

  /**
   * Sweep the journal directory for OTHER processes' abandoned files and return their unsettled
   * open records, removing each file so a record is reported once. Never rejects: an unreadable or
   * half-written file yields whatever parsed.
   */
  async recover(): Promise<JournalRecord[]> {
    if (!this.enabled) return [];
    await ensureFs();
    const fs = fsMod;
    if (!fs) return [];
    let names: string[];
    try {
      names = fs.readdirSync(this.dir);
    } catch {
      return [];
    }
    const out: JournalRecord[] = [];
    const now = Date.now();
    for (const name of names) {
      if (!name.startsWith(FILE_PREFIX) || !name.endsWith(FILE_SUFFIX)) continue;
      const full = `${this.dir}/${name}`;
      if (this.path && full === this.path) continue; // our own live journal
      try {
        if (now - fs.statSync(full).mtimeMs < this.orphanAfterMs) continue;
        out.push(...unsettled(fs.readFileSync(full, "utf-8")));
        fs.rmSync(full, { force: true });
      } catch {
        continue;
      }
    }
    return out;
  }

  // ---- internals ----
  private write(line: string): void {
    if (!fsMod) {
      // Still resolving the fs import (or there is none). Hold the line and flush when it lands —
      // in practice this resolves at client construction, long before the first provider call.
      this.pending.push(line);
      void ensureFs().then(() => this.flushPending());
      return;
    }
    this.flushPending();
    this.append(line);
  }

  private flushPending(): void {
    if (!this.pending.length) return;
    const lines = this.pending;
    this.pending = [];
    if (!fsMod) return; // no filesystem here — drop them rather than grow without bound
    for (const l of lines) this.append(l);
  }

  private append(line: string): void {
    const fs = fsMod;
    if (!fs || this.broken) return;
    try {
      if (!this.path) {
        fs.mkdirSync(this.dir, { recursive: true });
        const pid = typeof process !== "undefined" ? process.pid : 0;
        const rand = Math.random().toString(16).slice(2, 10);
        this.path = `${this.dir}/${FILE_PREFIX}${pid}-${rand}${FILE_SUFFIX}`;
      }
      fs.appendFileSync(this.path, `${line}\n`, "utf-8");
    } catch {
      this.broken = true; // one failure disables it; telemetry never fights the filesystem
    }
  }

  private truncate(): void {
    if (!fsMod || !this.path || this.broken) return;
    try {
      fsMod.truncateSync(this.path, 0);
    } catch {
      this.broken = true;
    }
  }
}

/** The open records in one journal file that never got a matching close. */
export function unsettled(text: string): JournalRecord[] {
  const opens = new Map<unknown, JournalRecord>();
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let rec: any;
    try {
      rec = JSON.parse(trimmed);
    } catch {
      // A kill mid-write leaves a partial last line. Everything before it is still good; dropping
      // only the torn record is the point of a line-per-record journal.
      continue;
    }
    if (!rec || typeof rec !== "object") continue;
    if (rec.o === "b") opens.set(rec.k, rec as JournalRecord);
    else if (rec.o === "e") opens.delete(rec.k);
  }
  return [...opens.values()];
}

/**
 * The `error` string an unsettled call is reported with. It says what is known (the call began, at
 * this time) and what is not (how it ended), rather than presenting a guess as an outcome.
 */
export function unsettledError(rec: JournalRecord): string {
  const when = typeof rec.t === "number" ? ` started ${new Date(rec.t).toISOString()}` : "";
  return (
    `unsettled span: the process that made this call exited or stalled before it reported an ` +
    `outcome${when}. Token counts and cost are unknown, not zero; latency is unknown, not the time ` +
    `until it was noticed. Recovered from the LightTrack client journal.`
  );
}
