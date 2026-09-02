/**
 * Read what an ingest response says about the project's limits.
 *
 * Parsing only. What a client *does* with the answer — pausing before it spends, honouring the
 * schedule, shedding its own traffic — is pre-spend admission, and lives elsewhere. Splitting the
 * two matters because the reading is the part that must be identical in every SDK: the same bytes
 * have to mean the same thing in Python, TypeScript and Rust, or a fleet mixing them enforces three
 * different policies. The cases are fixed in `clients/contract/fixtures/limits.json`.
 *
 * The recurring trap this closes is `null` vs `0`. A project with no limits reports no ratio at all;
 * a client that read the absence as `0.0` would believe it had infinite headroom. An unparseable
 * `Retry-After` is likewise unknown, not "retry immediately".
 */

/** What an ingest response says about limits. Every unknown is `null`, never a stand-in value. */
export interface LimitView {
  /** The event was recorded (2xx). */
  accepted: boolean;
  /** Refused for a usage limit (429) — a hard cap or graduated shedding. */
  rateLimited: boolean;
  /** Worst usage ratio among the rules that applied; `1.0` is at the cap. `null` when unknown. */
  usageRatio: number | null;
  /** Share of ingest currently being shed, `0.0`–`1.0`. `null` when nothing is throttling. */
  shedFraction: number | null;
  /** Seconds to wait, from `Retry-After`. `null` when absent or not a number (e.g. an HTTP-date). */
  retryAfterSecs: number | null;
  /** The API's stable error code (`rate_limited`, `bad_request`, …). `null` on success. */
  errorCode: string | null;
}

function finiteNumber(v: unknown): number | null {
  return typeof v === "number" && isFinite(v) ? v : null;
}

/** Header lookup that does not care about casing — HTTP does not guarantee it, and proxies rewrite it. */
function header(headers: Record<string, string> | Headers | undefined, name: string): string | undefined {
  if (!headers) return undefined;
  if (typeof (headers as Headers).get === "function") return (headers as Headers).get(name) ?? undefined;
  const want = name.toLowerCase();
  for (const [k, v] of Object.entries(headers as Record<string, string>)) {
    if (k.toLowerCase() === want) return v;
  }
  return undefined;
}

/**
 * Parse one ingest response into a {@link LimitView}. Pure and total: any shape of body, including
 * none at all, yields a view rather than an exception.
 */
export function parseLimitView(
  status: number,
  headers?: Record<string, string> | Headers,
  body?: unknown,
): LimitView {
  const obj = body && typeof body === "object" ? (body as Record<string, unknown>) : undefined;
  const err = obj?.error && typeof obj.error === "object" ? (obj.error as Record<string, unknown>) : undefined;
  const raw = header(headers, "retry-after");
  // Deliberately integer-only: `Retry-After` may also be an HTTP-date, and a half-parsed date that
  // came out as 0 would tell the client to hammer the endpoint it was just asked to back off from.
  const retry = raw != null && /^\d+$/.test(raw.trim()) ? Number(raw.trim()) : null;

  return {
    accepted: status >= 200 && status < 300,
    rateLimited: status === 429,
    usageRatio: finiteNumber(obj?.usage_ratio),
    shedFraction: finiteNumber(obj?.shed_fraction),
    retryAfterSecs: retry,
    errorCode: typeof err?.code === "string" ? err.code : null,
  };
}
