/**
 * Read what an ingest response says about the project's limits.
 *
 * Parsing only. What a client *does* with the answer — pausing before it spends, honouring the
 * schedule, shedding its own traffic — is pre-spend admission, and lives in `admission.ts`.
 * Splitting the two matters because the reading is the part that must be identical in every SDK:
 * the same bytes have to mean the same thing in Python, TypeScript and Rust, or a fleet mixing them
 * enforces three different policies. The cases are fixed in `clients/contract/fixtures/limits.json`.
 *
 * The recurring trap this closes is `null` vs `0`. A project with no limits reports no ratio at all;
 * a client that read the absence as `0.0` would believe it had infinite headroom. An unparseable
 * `Retry-After` is likewise unknown, not "retry immediately".
 *
 * Signals arrive on two channels. `POST /v1/events` carries them as body fields. The batch door
 * answers multi-status (the project's position is not a property of item 7) and the OTLP door
 * answers in the exporter's own envelope, so neither has a body field to put them in — both send
 * `X-LightTrack-Usage-Ratio` / `-Shed-Fraction` / `-Retry-After` instead, and so does the 429, which
 * has no `IngestResponse` body at all. The body wins where both are present.
 */

/** The dimension the binding rule applies to. Absent means the binding rule is project-wide. */
export interface BindingScope {
  /** `provider` | `model` | `name` | `api_key` | `customer`. */
  kind: string;
  value: string;
}

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
  /**
   * Which rule the ratio belongs to. `null` = project-wide (or unknown). `0.94` alone says stop
   * everything; `0.94` on `model=gpt-4o` says route the next call elsewhere and keep working.
   */
  bindingScope: BindingScope | null;
  /**
   * Id of the binding rule. The server's shed decision is a hash of `(rule_id, event_id)`, so this
   * is what lets a client reproduce it rather than merely run the same function.
   */
  bindingRule: string | null;
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

/** A header that should be a number, or `null`. Deliberately total: junk reads as unknown. */
function headerNumber(headers: Record<string, string> | Headers | undefined, name: string): number | null {
  const raw = header(headers, name);
  if (raw == null) return null;
  const n = Number(raw.trim());
  return isFinite(n) ? n : null;
}

/** Integer seconds from a `Retry-After`-shaped header value. */
function retryAfterSecs(raw: string | undefined): number | null {
  // Deliberately integer-only: `Retry-After` may also be an HTTP-date, and a half-parsed date that
  // came out as 0 would tell the client to hammer the endpoint it was just asked to back off from.
  return raw != null && /^\d+$/.test(raw.trim()) ? Number(raw.trim()) : null;
}

function bindingScopeOf(v: unknown): BindingScope | null {
  if (!v || typeof v !== "object") return null;
  const o = v as Record<string, unknown>;
  return typeof o.kind === "string" && typeof o.value === "string" ? { kind: o.kind, value: o.value } : null;
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
  // The standard header is the contract; the `X-LightTrack-` mirror is the copy that survives a
  // proxy which dropped the original. Never the other way round.
  const retry =
    retryAfterSecs(header(headers, "retry-after")) ??
    retryAfterSecs(header(headers, "x-lighttrack-retry-after"));

  return {
    accepted: status >= 200 && status < 300,
    rateLimited: status === 429,
    usageRatio: finiteNumber(obj?.usage_ratio) ?? headerNumber(headers, "x-lighttrack-usage-ratio"),
    shedFraction: finiteNumber(obj?.shed_fraction) ?? headerNumber(headers, "x-lighttrack-shed-fraction"),
    retryAfterSecs: retry,
    errorCode: typeof err?.code === "string" ? err.code : null,
    bindingScope: bindingScopeOf(obj?.binding_scope),
    bindingRule: typeof obj?.binding_rule === "string" ? obj.binding_rule : null,
  };
}
