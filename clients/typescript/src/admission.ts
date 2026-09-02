/**
 * Pre-spend admission: decide, before the provider call, whether to make it at all.
 *
 * Every cap LightTrack has is record-side. The server refuses to *record* a call that already cost
 * money — the money is gone by the time the 429 arrives. The signals to do better were already on
 * the wire (`usage_ratio`, `shed_fraction`, `Retry-After`, and now the `X-LightTrack-*` headers);
 * this module is what finally reads them and acts.
 *
 * Three rules shape the design:
 *
 * 1. **Pure.** {@link AdmissionCache.admit} performs no I/O and reads no clock it was not handed.
 *    A decision that could block on a network call would put LightTrack on the critical path of
 *    every LLM call in the host app — precisely the cost `docs/ARCHITECTURE.md` §4 deferred the
 *    inline gateway to avoid.
 * 2. **Fails open.** No observation, or an observation older than the TTL, admits. A telemetry
 *    client that stops an app's LLM calls because it is itself confused is worse than one that
 *    records nothing.
 * 3. **Scoped.** A cap on the `summarize` use-case must stop `summarize` and nothing else. Views
 *    are cached per binding scope, which is why the server names it.
 *
 * The verdicts are fixed across all three SDKs in `clients/contract/fixtures/limits.json`.
 */

import type { BindingScope, LimitView } from "./limits.ts";

/** How long a cached view is still evidence. Past it, {@link AdmissionCache.admit} admits and says so. */
export const DEFAULT_ADMISSION_TTL_MS = 30_000;

/** What the enforcing wrappers do with a refusal. */
export type Enforce = "block" | "warn" | "off";

/** Why a call was refused. `null` when it was admitted. */
export type AdmitReason = "retry_after" | "at_cap" | "shed";

/** The verdict on one prospective call. */
export interface Admit {
  /** Whether the provider call should be made. */
  ok: boolean;
  /** `null` when ok; otherwise which condition refused it. */
  reason: AdmitReason | null;
  /** Only set for `retry_after` — a client must not invent a back-off the server never promised. */
  retryAfterSecs: number | null;
  /** The view is past its TTL, so this verdict was taken without current evidence (and admits). */
  stale: boolean;
}

/** What {@link AdmissionCache.admit} is being asked about. */
export interface AdmitQuery {
  /** Use-case of the call, matched against a `name`-scoped view. */
  name?: string | null;
  /** Id the call would be recorded under — needed to decide the shed lottery. */
  eventId?: string | null;
  /** Wall clock, injected so the decision is a pure function. Defaults to `Date.now()`. */
  nowMs?: number;
}

/** One cached view, keyed by the scope it describes. */
interface Entry {
  usageRatio: number | null;
  shedFraction: number | null;
  /** Absolute deadline of a 429's advertised wait, or `null`. */
  retryAfterUntilMs: number | null;
  bindingScope: BindingScope | null;
  bindingRule: string | null;
  refreshedAtMs: number;
}

/**
 * Map `(rule, event)` to a stable point in `[0, 1)` — the server's shed lottery (ARCHITECTURE §7c),
 * ported from `lighttrack_core::shed_ticket`.
 *
 * A port rather than a re-invention on purpose: a different hash would still shed proportionally and
 * still look right in aggregate, while disagreeing with the server on every individual event. The
 * values are pinned in the `shed_lottery` fixture, which the Rust runner checks against the server's
 * own function. Note the multiplier below is the server's own `0x1000000001b3`, **not** the textbook
 * FNV prime `0x100000001b3`: reaching for the standard constant yields a perfectly good hash that
 * disagrees with the server on every single event.
 */
export function shedTicket(ruleId: string, eventId: string): number {
  const MASK = (1n << 64n) - 1n;
  let h = 0xcbf29ce484222325n;
  const enc = new TextEncoder();
  const bytes = [...enc.encode(ruleId), 0x1f, ...enc.encode(eventId)];
  for (const b of bytes) {
    h = (h ^ BigInt(b)) & MASK;
    h = (h * 0x1000000001b3n) & MASK;
  }
  // FNV mixes its low bits well and its high ones poorly on short inputs, and we want the top 53.
  h = (h ^ (h >> 30n)) & MASK;
  h = (h * 0xbf58476d1ce4e5b9n) & MASK;
  h = (h ^ (h >> 27n)) & MASK;
  h = (h * 0x94d049bb133111ebn) & MASK;
  h = (h ^ (h >> 31n)) & MASK;
  return Number(h >> 11n) / 2 ** 53;
}

const PROJECT_WIDE = "";

function scopeKey(scope: BindingScope | null | undefined): string {
  return scope ? `${scope.kind}=${scope.value}` : PROJECT_WIDE;
}

/**
 * The per-client store of what the server last said, and the decision taken from it.
 *
 * One entry per binding scope: the project-wide view under `""`, a `name`-scoped view under
 * `name=<use-case>`, and so on. Nothing is ever evicted by count — the number of distinct scopes a
 * project's rules can name is small and operator-authored.
 */
export class AdmissionCache {
  private readonly ttlMs: number;
  private readonly views = new Map<string, Entry>();

  constructor(opts: { ttlMs?: number } = {}) {
    this.ttlMs = opts.ttlMs ?? DEFAULT_ADMISSION_TTL_MS;
  }

  /** Fold one parsed ingest response into the cache. */
  observe(view: LimitView, nowMs: number = Date.now()): void {
    const key = scopeKey(view.bindingScope);
    const prior = this.views.get(key);
    // Only a 429 arms the wait. A 503 carries `Retry-After` too, but it means the *ingest endpoint*
    // is saturated — pausing the app's LLM calls over that would be the observability tool causing
    // the outage it exists to observe. And a 2xx is the server saying the refusal is over, which
    // outranks a schedule the client is still holding.
    let until: number | null;
    if (view.accepted) until = null;
    else if (view.rateLimited && view.retryAfterSecs != null) until = nowMs + view.retryAfterSecs * 1000;
    else until = prior?.retryAfterUntilMs ?? null;

    this.views.set(key, {
      usageRatio: view.usageRatio,
      shedFraction: view.shedFraction,
      retryAfterUntilMs: until,
      bindingScope: view.bindingScope,
      bindingRule: view.bindingRule,
      refreshedAtMs: nowMs,
    });
  }

  /** Drop everything (a key rotation, a project switch — anything that invalidates the evidence). */
  clear(): void {
    this.views.clear();
  }

  /**
   * Decide one prospective call. Pure: no I/O, and no clock beyond `q.nowMs`.
   *
   * A `name` is answered from that use-case's own view when the server has named one, and from the
   * project-wide view otherwise — applying the worst rule in the project to every call is how a
   * scoped budget turns into a project-wide outage.
   */
  admit(q: AdmitQuery = {}): Admit {
    const now = q.nowMs ?? Date.now();
    const entry =
      (q.name ? this.views.get(`name=${q.name}`) : undefined) ?? this.views.get(PROJECT_WIDE);
    if (!entry) return admitted(false);

    // The advertised wait is an absolute deadline, so it is honoured even past the TTL: the server
    // told us when to come back, and that instruction does not go stale, it expires.
    if (entry.retryAfterUntilMs != null && now < entry.retryAfterUntilMs) {
      return {
        ok: false,
        reason: "retry_after",
        retryAfterSecs: Math.ceil((entry.retryAfterUntilMs - now) / 1000),
        stale: false,
      };
    }
    if (now - entry.refreshedAtMs > this.ttlMs) return admitted(true);
    if (entry.usageRatio != null && entry.usageRatio >= 1.0) {
      return { ok: false, reason: "at_cap", retryAfterSecs: null, stale: false };
    }
    if (
      entry.shedFraction != null &&
      entry.shedFraction > 0 &&
      q.eventId != null &&
      shedTicket(entry.bindingRule ?? "", q.eventId) < entry.shedFraction
    ) {
      return { ok: false, reason: "shed", retryAfterSecs: null, stale: false };
    }
    return admitted(false);
  }
}

function admitted(stale: boolean): Admit {
  return { ok: true, reason: null, retryAfterSecs: null, stale };
}

/**
 * The refusal an enforcing wrapper throws instead of making the provider call.
 *
 * Typed, because the host app has to be able to tell "your budget said no" from a provider outage:
 * the first is a decision it may want to degrade around (a smaller model, a cached answer, a queue),
 * the second is a retry.
 */
export class LightTrackBudgetExceeded extends Error {
  readonly reason: AdmitReason | null;
  readonly retryAfterSecs: number | null;

  constructor(verdict: Admit, detail = "") {
    super(
      `LightTrack refused this call before it was made (${verdict.reason ?? "unknown"})` +
        (verdict.retryAfterSecs != null ? `; retry in ${verdict.retryAfterSecs}s` : "") +
        (detail ? `. ${detail}` : ""),
    );
    this.name = "LightTrackBudgetExceeded";
    this.reason = verdict.reason;
    this.retryAfterSecs = verdict.retryAfterSecs;
  }
}

/**
 * Collapse the `statuses` of `GET /v1/limits/status` into one view, the same way the ingest doors
 * do: worst ratio, strongest shed, and the identity of the worst rule.
 */
export function viewFromStatuses(statuses: unknown): LimitView | null {
  if (!Array.isArray(statuses) || statuses.length === 0) return null;
  let worst: any;
  let ratio: number | null = null;
  let shed: number | null = null;
  for (const s of statuses as any[]) {
    const r = typeof s?.ratio === "number" ? s.ratio : null;
    if (r != null && (ratio == null || r > ratio)) {
      ratio = r;
      worst = s;
    }
    const f = typeof s?.shed_fraction === "number" ? s.shed_fraction : 0;
    if (f > 0 && (shed == null || f > shed)) shed = f;
  }
  const scope = worst?.scope && typeof worst.scope === "object" ? worst.scope : undefined;
  const kind = scope ? Object.keys(scope)[0] : undefined;
  return {
    accepted: true,
    rateLimited: false,
    usageRatio: ratio,
    shedFraction: shed,
    retryAfterSecs: null,
    errorCode: null,
    bindingScope: kind && typeof scope[kind] === "string" ? { kind, value: scope[kind] } : null,
    bindingRule: typeof worst?.rule_id === "string" ? worst.rule_id : null,
  };
}
