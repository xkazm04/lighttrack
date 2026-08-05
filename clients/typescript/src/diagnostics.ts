/**
 * Make a failed send *visible* without ever making it *throw*.
 *
 * The client is fire-and-forget by contract: telemetry must never break the host app, so `post`
 * ends in `.catch(() => undefined)`. Swallowing everything, though, also swallowed the failure every
 * first-time user hits — follow the README with no project configured, the API answers
 * `400 project_id is required`, the event vanishes, and nothing at all is printed.
 *
 * So: still never throw, never block, never touch stdout (the host app may be speaking a protocol on
 * it — `console.warn` goes to stderr in Node) — but write one actionable line, rate-limited per error
 * kind so a tight loop of failing calls warns once rather than thousands of times.
 *
 * Silence it with `LIGHTTRACK_QUIET=1` or `new LightTrack({ quiet: true })`.
 */

export const PREFIX = "[lighttrack]";
/** One line per error kind per this many ms. A persistent outage still re-warns (reporting what was
 *  suppressed) instead of going quiet forever after the first line. */
export const COOLDOWN_MS = 60_000;
const SILENCE_HINT = "silence these warnings with LIGHTTRACK_QUIET=1 or new LightTrack({ quiet: true })";
const TRUTHY = ["1", "true", "yes", "on"];

function envVar(name: string): string | undefined {
  return typeof process !== "undefined" && process.env ? process.env[name] : undefined;
}

export function envQuiet(): boolean {
  return TRUTHY.includes((envVar("LIGHTTRACK_QUIET") ?? "").trim().toLowerCase());
}

export function truncate(s: string, limit = 200): string {
  const t = String(s).split(/\s+/).join(" ");
  return t.length <= limit ? t : t.slice(0, limit - 3) + "...";
}

/** Rate-limited warner. Every method is exception-proof: a diagnostic must never become the failure
 *  it is reporting. */
export class Diagnostics {
  quiet: boolean;
  cooldownMs: number;
  /** Lines actually written (test hook). */
  emitted = 0;
  /** Lines withheld by the rate limiter (test hook). */
  suppressed = 0;
  private seen = new Map<string, { last: number; held: number }>();

  constructor(opts: { quiet?: boolean; cooldownMs?: number } = {}) {
    this.quiet = opts.quiet ?? envQuiet();
    this.cooldownMs = opts.cooldownMs ?? COOLDOWN_MS;
  }

  /** Emit `message` at most once per `kind` per cooldown window. */
  warn(kind: string, message: string): void {
    try {
      if (this.quiet) return;
      const now = Date.now();
      const prev = this.seen.get(kind);
      if (prev && now - prev.last < this.cooldownMs) {
        prev.held += 1;
        this.suppressed += 1;
        return;
      }
      const held = prev?.held ?? 0;
      this.seen.set(kind, { last: now, held: 0 });
      this.emitted += 1;
      const repeat = held ? ` [${held} more suppressed in the last ${Math.round(this.cooldownMs / 1000)}s]` : "";
      const hint = this.emitted === 1 ? `\n  ${PREFIX} ${SILENCE_HINT}` : "";
      console.warn(`${PREFIX} ${message}${repeat}${hint}`);
    } catch {
      /* a diagnostic must never break the host app either */
    }
  }
}

/**
 * The first-run trap: no project *and* no API key, so the server cannot infer one and will reject
 * every event with 400. Detected before the network call, so the user is told immediately.
 *
 * Messages stay ASCII-only: they land in whatever console the host app has, and a cp1252 Windows
 * terminal turns a stray em dash into mojibake.
 */
export function noProjectMessage(baseUrl: string): string {
  return (
    "events are being dropped: no project is configured, and without an API key the server cannot " +
    "infer one, so it will reject them with HTTP 400 'project_id is required'. Fix: set " +
    "LIGHTTRACK_PROJECT=<your-project-id> (or new LightTrack({ project: '...' })), or set " +
    `LIGHTTRACK_KEY to a project API key, which pins the project server-side. Target: ${baseUrl}`
  );
}

export interface FailureContext {
  status?: number;
  hasProject?: boolean;
  hasKey?: boolean;
}

export function sendFailureMessage(baseUrl: string, path: string, detail: string, ctx: FailureContext = {}): string {
  const hint = failureHint(baseUrl, ctx);
  return `event not sent to ${baseUrl}${path}: ${detail}.` + (hint ? ` ${hint}` : "");
}

function failureHint(baseUrl: string, { status, hasProject, hasKey }: FailureContext): string {
  if (status == null) {
    return (
      `Is a LightTrack server running and reachable at ${baseUrl}? Check LIGHTTRACK_URL. ` +
      "Events are dropped while it is unreachable."
    );
  }
  if (status === 400 && !hasProject) {
    // The same trap as `noProjectMessage`, reached the slow way: a key was set (an *admin* key,
    // which pins no project) so the preflight check passed and the server did the rejecting.
    return (
      "The server has no project for this event. Fix: set LIGHTTRACK_PROJECT=<your-project-id> " +
      "(or new LightTrack({ project: '...' })), or use a *project* API key in LIGHTTRACK_KEY; " +
      "an admin key does not imply a project."
    );
  }
  if (status === 400) return "The event was rejected as invalid: check provider / model / usage.";
  if (status === 401 || status === 403) {
    return hasKey
      ? "The key was rejected. Set LIGHTTRACK_KEY to a valid project or admin key (or new LightTrack({ apiKey: '...' }))."
      : "This server requires authentication. Set LIGHTTRACK_KEY to a project API key.";
  }
  if (status === 404) return `No such endpoint - is LIGHTTRACK_URL (${baseUrl}) pointing at a LightTrack API?`;
  if (status === 429) return "The project is over a configured usage limit, so ingest is being refused.";
  if (status >= 500) return "The LightTrack server errored; events are dropped until it recovers.";
  return "";
}
