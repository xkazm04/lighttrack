/**
 * LightTrack TypeScript/JavaScript client — fire-and-forget LLM event ingestion.
 *
 * Wrap your OpenAI / Anthropic / Gemini results and POST a normalized event to the LightTrack API.
 * Best-effort by design: `track*` never throws and never blocks (the POST is not awaited). Uses the
 * global `fetch` (Node 18+ / browsers); zero runtime dependencies.
 *
 *   import { LightTrack } from "lighttrack-client";
 *   const lt = new LightTrack();                  // LIGHTTRACK_URL + LIGHTTRACK_KEY from env (Node)
 *   const resp = await openai.chat.completions.create(...);
 *   lt.trackOpenAI(resp, { latencyMs: 120 });
 *   await lt.flush();                             // await in-flight sends before exit
 */

export type ProviderName = "openai" | "anthropic" | "google" | (string & {});

export interface TrackOptions {
  inputTokens?: number;
  outputTokens?: number;
  cachedInput?: number;
  operation?: "chat" | "completion" | "embedding" | string;
  latencyMs?: number;
  status?: "success" | "error" | "timeout";
  error?: string;
  input?: unknown;
  output?: unknown;
  tags?: string[];
  traceId?: string;
  metadata?: Record<string, unknown>;
  /** Project id — for dev mode (no key) or an admin key; a project key overrides it server-side. */
  project?: string;
}

export interface LightTrackConfig {
  baseUrl?: string;
  apiKey?: string;
  project?: string;
  source?: string;
  tags?: string[];
  enabled?: boolean;
  timeoutMs?: number;
}

const DEFAULT_URL = "http://127.0.0.1:8787";

const PROVIDER_ALIASES: Record<string, string> = {
  openai: "openai", azure: "openai", azure_openai: "openai", oai: "openai",
  anthropic: "anthropic", claude: "anthropic",
  google: "google", gemini: "google", vertex: "google", vertexai: "google", genai: "google",
};

function env(name: string): string | undefined {
  return typeof process !== "undefined" && process.env ? process.env[name] : undefined;
}

function normProvider(p: string): string {
  const s = String(p).trim().toLowerCase();
  return PROVIDER_ALIASES[s] ?? s;
}

function num(v: unknown): number | undefined {
  return typeof v === "number" && isFinite(v) ? v : undefined;
}

/** Extract (model, input, output, cached) from an OpenAI chat/responses object. */
export function extractOpenAI(resp: any): [string | undefined, number, number, number | undefined] {
  const u = resp?.usage ?? {};
  const input = num(u.prompt_tokens) ?? num(u.input_tokens) ?? 0;
  const output = num(u.completion_tokens) ?? num(u.output_tokens) ?? 0;
  const cached = num(u.prompt_tokens_details?.cached_tokens);
  return [resp?.model, input, output, cached];
}

/** Extract (model, input, output, cached) from an Anthropic messages object. */
export function extractAnthropic(resp: any): [string | undefined, number, number, number | undefined] {
  const u = resp?.usage ?? {};
  return [resp?.model, num(u.input_tokens) ?? 0, num(u.output_tokens) ?? 0, num(u.cache_read_input_tokens)];
}

/** Extract (model, input, output, cached) from a Gemini generateContent object. */
export function extractGemini(resp: any): [string | undefined, number, number, number | undefined] {
  const u = resp?.usageMetadata ?? resp?.usage_metadata ?? {};
  const input = num(u.promptTokenCount) ?? num(u.prompt_token_count) ?? 0;
  const output = num(u.candidatesTokenCount) ?? num(u.candidates_token_count) ?? 0;
  const cached = num(u.cachedContentTokenCount) ?? num(u.cached_content_token_count);
  return [resp?.modelVersion ?? resp?.model_version, input, output, cached];
}

export class LightTrack {
  private baseUrl: string;
  private apiKey?: string;
  private project?: string;
  private source?: string;
  private defaultTags: string[];
  private enabled: boolean;
  private timeoutMs: number;
  private inflight: Set<Promise<void>> = new Set();

  constructor(cfg: LightTrackConfig = {}) {
    this.baseUrl = (cfg.baseUrl ?? env("LIGHTTRACK_URL") ?? DEFAULT_URL).replace(/\/+$/, "");
    this.apiKey = cfg.apiKey ?? env("LIGHTTRACK_KEY");
    this.project = cfg.project ?? env("LIGHTTRACK_PROJECT");
    this.source = cfg.source;
    this.defaultTags = cfg.tags ?? [];
    this.enabled = cfg.enabled ?? true;
    this.timeoutMs = cfg.timeoutMs ?? 2000;
  }

  /** Record one LLM call. Returns immediately; the send is fire-and-forget. */
  track(provider: ProviderName, model: string | undefined, opts: TrackOptions = {}): void {
    if (!this.enabled) return;
    const usage: Record<string, number> = {
      input: Math.trunc(opts.inputTokens ?? 0),
      output: Math.trunc(opts.outputTokens ?? 0),
    };
    if (opts.cachedInput != null) usage.cached_input = Math.trunc(opts.cachedInput);

    const ev: Record<string, unknown> = {
      provider: normProvider(provider),
      model: model ?? "unknown",
      usage,
    };
    const pid = opts.project ?? this.project;
    if (pid) ev.project_id = pid;
    if (opts.operation) ev.operation = opts.operation;
    if (opts.latencyMs != null) ev.latency_ms = Math.trunc(opts.latencyMs);
    let status = opts.status;
    if (opts.error) {
      ev.error = opts.error;
      status = status ?? "error";
    }
    if (status) ev.status = status;
    if (opts.input !== undefined) ev.input = opts.input;
    if (opts.output !== undefined) ev.output = opts.output;
    const tags = [...this.defaultTags, ...(opts.tags ?? [])];
    if (tags.length) ev.tags = tags;
    if (opts.traceId) ev.trace_id = opts.traceId;
    if (this.source) ev.source = this.source;
    if (opts.metadata) ev.metadata = opts.metadata;

    this.send(ev);
  }

  trackOpenAI(response: any, opts: TrackOptions = {}): void {
    const [model, input, output, cached] = extractOpenAI(response);
    this.track("openai", model, { inputTokens: input, outputTokens: output, cachedInput: cached, ...opts });
  }

  trackAnthropic(response: any, opts: TrackOptions = {}): void {
    const [model, input, output, cached] = extractAnthropic(response);
    this.track("anthropic", model, { inputTokens: input, outputTokens: output, cachedInput: cached, ...opts });
  }

  trackGemini(response: any, model?: string, opts: TrackOptions = {}): void {
    const [m, input, output, cached] = extractGemini(response);
    this.track("google", model ?? m, { inputTokens: input, outputTokens: output, cachedInput: cached, ...opts });
  }

  /** Time a call and track on `end()`: `const s = lt.span("openai","gpt-4o"); ...; s.endOpenAI(resp)`. */
  span(provider: ProviderName, model?: string, opts: TrackOptions = {}): Span {
    return new Span(this, provider, model, opts);
  }

  /** Await all in-flight sends (call before process exit). */
  async flush(): Promise<void> {
    await Promise.allSettled([...this.inflight]);
  }

  private send(ev: Record<string, unknown>): void {
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (this.apiKey) headers["Authorization"] = `Bearer ${this.apiKey}`;
    const ac = typeof AbortController !== "undefined" ? new AbortController() : undefined;
    const timer = ac ? setTimeout(() => ac.abort(), this.timeoutMs) : undefined;
    const p = fetch(`${this.baseUrl}/v1/events`, {
      method: "POST",
      headers,
      body: JSON.stringify(ev),
      signal: ac?.signal,
    })
      .then(() => undefined)
      .catch(() => undefined) // best-effort: telemetry must never break the host app
      .finally(() => {
        if (timer) clearTimeout(timer);
        this.inflight.delete(p);
      });
    this.inflight.add(p);
  }
}

export class Span {
  private client: LightTrack;
  private provider: ProviderName;
  private opts: TrackOptions;
  private startedAt: number;
  private usage: { inputTokens: number; outputTokens: number; cachedInput?: number } = {
    inputTokens: 0,
    outputTokens: 0,
  };
  private model?: string;

  constructor(client: LightTrack, provider: ProviderName, model: string | undefined, opts: TrackOptions) {
    this.client = client;
    this.provider = provider;
    this.opts = opts;
    this.model = model;
    this.startedAt = Date.now();
  }

  setUsage(inputTokens: number, outputTokens: number, cachedInput?: number): this {
    this.usage = { inputTokens, outputTokens, cachedInput };
    return this;
  }

  setOpenAI(resp: any): this {
    const [m, i, o, c] = extractOpenAI(resp);
    this.model = this.model ?? m;
    return this.setUsage(i, o, c);
  }

  setAnthropic(resp: any): this {
    const [m, i, o, c] = extractAnthropic(resp);
    this.model = this.model ?? m;
    return this.setUsage(i, o, c);
  }

  setGemini(resp: any): this {
    const [m, i, o, c] = extractGemini(resp);
    this.model = this.model ?? m;
    return this.setUsage(i, o, c);
  }

  /** Finish the span: measure latency and track. Pass an error to record a failed call. */
  end(error?: unknown): void {
    this.client.track(this.provider, this.model, {
      ...this.opts,
      latencyMs: Date.now() - this.startedAt,
      inputTokens: this.usage.inputTokens,
      outputTokens: this.usage.outputTokens,
      cachedInput: this.usage.cachedInput,
      status: error ? "error" : this.opts.status,
      error: error ? String(error) : this.opts.error,
    });
  }
}
