/**
 * One-line auto-instrumentation for the official OpenAI / Anthropic / Gemini SDK clients.
 *
 * Wrap a client once and every call it makes is captured automatically — model, token usage,
 * latency, and full trace linkage — instead of hand-writing a `track*` per call:
 *
 *   import { wrapOpenAI } from "lighttrack-client";
 *   const openai = wrapOpenAI(new OpenAI());                 // drop-in: same client, now observed
 *   const resp = await openai.chat.completions.create({ ... });
 *
 * Trace context: calls made inside `withTrace(fn)` share one `trace_id`; `withSpan(fn)` nests their
 * `parent_span_id`, so multi-step / agentic apps feed straight into the trace view. A call with no
 * active trace becomes its own single-span trace.
 *
 * Best-effort, like the rest of the client: instrumentation never throws into your app. A failing
 * provider call is still recorded as a failed span before its error rethrows.
 */

import { LightTrack, extractAnthropic, extractGemini, extractOpenAI } from "./index.ts";

type Extract = (resp: any) => [string | undefined, number, number, number | undefined];

// ---- trace context ---------------------------------------------------------
//
// A single module-global context, swapped around the awaited callback and restored in `finally`.
// This propagates correctly through sequential `await`s inside one trace. For strictly concurrent,
// interleaved traces in the same process, pass an explicit `traceId` to keep them isolated.

interface Ctx {
  traceId?: string;
  parentSpanId?: string;
}

let ctx: Ctx = {};

/** A random hex id (uses `crypto.randomUUID` when available; falls back to `Math.random`). */
export function randomId(): string {
  const g = globalThis as any;
  if (g.crypto?.randomUUID) return g.crypto.randomUUID().replace(/-/g, "");
  let s = "";
  for (let i = 0; i < 32; i++) s += ((Math.random() * 16) | 0).toString(16);
  return s;
}

/** The trace id wrapped calls will use right now (undefined if no trace is open). */
export function currentTraceId(): string | undefined {
  return ctx.traceId;
}

/** The span id wrapped calls will attach to as their parent (undefined at trace root). */
export function currentSpanId(): string | undefined {
  return ctx.parentSpanId;
}

/** Run `fn` inside a trace; every wrapped call it makes shares one `trace_id`. Returns `fn`'s result. */
export async function withTrace<T>(fn: () => T | Promise<T>, opts: { traceId?: string } = {}): Promise<T> {
  const prev = ctx;
  ctx = { traceId: opts.traceId ?? randomId(), parentSpanId: undefined };
  try {
    return await fn();
  } finally {
    ctx = prev;
  }
}

/** Run `fn` inside a logical span; wrapped calls get this span as their `parent_span_id`. */
export async function withSpan<T>(fn: () => T | Promise<T>, opts: { spanId?: string } = {}): Promise<T> {
  const prev = ctx;
  ctx = { traceId: prev.traceId ?? randomId(), parentSpanId: opts.spanId ?? randomId() };
  try {
    return await fn();
  } finally {
    ctx = prev;
  }
}

// ---- default client --------------------------------------------------------

let defaultLt: LightTrack | undefined;

function defaultClient(): LightTrack {
  if (!defaultLt) defaultLt = new LightTrack();
  return defaultLt;
}

// ---- call tracking ---------------------------------------------------------

function record(
  lt: LightTrack,
  provider: string,
  extract: Extract,
  operation: string,
  fallbackModel: string | undefined,
  latencyMs: number,
  resp: any,
  error: unknown,
  stream: boolean,
): void {
  try {
    let model = fallbackModel;
    let input = 0;
    let output = 0;
    let cached: number | undefined;
    if (resp != null && !stream) {
      const [m, i, o, c] = extract(resp);
      model = m ?? fallbackModel;
      input = i;
      output = o;
      cached = c;
    }
    lt.track(provider, model, {
      inputTokens: input,
      outputTokens: output,
      cachedInput: cached,
      operation,
      latencyMs,
      status: error != null ? "error" : undefined,
      error: error != null ? String(error) : undefined,
      traceId: currentTraceId() ?? randomId(),
      spanId: randomId(),
      parentSpanId: currentSpanId(),
      tags: stream ? ["stream"] : undefined,
    });
  } catch {
    /* instrumentation must never break the host app */
  }
}

/** Replace `obj[key]` with a timing+tracking wrapper. Handles sync and Promise-returning methods. */
function patch(obj: any, key: string, lt: LightTrack, provider: string, extract: Extract, operation: string): void {
  const orig = obj?.[key];
  if (typeof orig !== "function" || orig.__lighttrack) return;
  const bound = orig.bind(obj);
  const wrapped: any = function (...args: any[]): any {
    const t0 = Date.now();
    const first = args[0] && typeof args[0] === "object" ? args[0] : undefined;
    const stream = !!first?.stream;
    const fallback = first?.model as string | undefined;
    let ret: any;
    try {
      ret = bound(...args);
    } catch (e) {
      record(lt, provider, extract, operation, fallback, Date.now() - t0, null, e, stream);
      throw e;
    }
    if (ret && typeof ret.then === "function") {
      return ret.then(
        (resp: any) => {
          record(lt, provider, extract, operation, fallback, Date.now() - t0, resp, null, stream);
          return resp;
        },
        (err: any) => {
          record(lt, provider, extract, operation, fallback, Date.now() - t0, null, err, stream);
          throw err;
        },
      );
    }
    record(lt, provider, extract, operation, fallback, Date.now() - t0, ret, null, stream);
    return ret;
  };
  wrapped.__lighttrack = true;
  try {
    obj[key] = wrapped;
  } catch {
    /* read-only property — skip */
  }
}

// ---- public wrappers -------------------------------------------------------

/** Instrument an OpenAI client instance (chat, responses, embeddings). Returns the same object. */
export function wrapOpenAI<T>(client: T, lt: LightTrack = defaultClient()): T {
  const c = client as any;
  patch(c?.chat?.completions, "create", lt, "openai", extractOpenAI, "chat");
  patch(c?.responses, "create", lt, "openai", extractOpenAI, "chat");
  patch(c?.embeddings, "create", lt, "openai", extractOpenAI, "embedding");
  return client;
}

/** Instrument an Anthropic client instance (`messages.create`). Returns the same object. */
export function wrapAnthropic<T>(client: T, lt: LightTrack = defaultClient()): T {
  const c = client as any;
  patch(c?.messages, "create", lt, "anthropic", extractAnthropic, "chat");
  return client;
}

/** Instrument a Google GenAI client instance (`models.generateContent`). Returns the same object. */
export function wrapGemini<T>(client: T, lt: LightTrack = defaultClient()): T {
  const c = client as any;
  patch(c?.models, "generateContent", lt, "google", extractGemini, "chat");
  return client;
}

/** Auto-detect which of the three known SDK clients this is and instrument it. Returns the same object. */
export function wrap<T>(client: T, lt: LightTrack = defaultClient()): T {
  const c = client as any;
  if (c?.chat?.completions || c?.responses || c?.embeddings) wrapOpenAI(client, lt);
  if (typeof c?.messages?.create === "function") wrapAnthropic(client, lt);
  if (typeof c?.models?.generateContent === "function") wrapGemini(client, lt);
  return client;
}
