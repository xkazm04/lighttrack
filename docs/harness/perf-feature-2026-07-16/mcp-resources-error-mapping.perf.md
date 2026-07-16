# Performance Optimizer — MCP Resources & Error Mapping

> Total: 3
> Critical: 1 | High: 0 | Medium: 2 | Low: 0

## 1. `resources/read` attaches the full untruncated raw JSON alongside the Markdown, defeating every payload budget the renderers enforce

- **Severity**: Critical
- **Category**: payload-size / unbounded-result
- **File**: `crates/mcp/src/resources.rs:58-68`
- **Scenario**: An agent attaches `lighttrack://trace/{id}` for a real agentic trace — say 30 spans, each an `LlmEvent` with `input`/`output` payloads persisted (`redaction: none`, the default set by `create_project`). `resources/read` returns two `contents` items: the rendered Markdown, and `serde_json::to_string_pretty(&body)` — the entire `TraceDetail`, every span's full prompt and completion. Same for `lighttrack://event/{id}`.
- **Root cause**: The API's `GET /v1/traces/:id` returns `TraceDetail` = `Trace` + scores, and `Trace.spans: Vec<TraceSpan>` where `TraceSpan.event` is the complete `LlmEvent` including `input: Option<Value>` and `output: Option<Value>` (`crates/core/src/trace.rs:35-46`, `crates/core/src/event.rs:155-159`). The render layer is deliberately budgeted against exactly this: `traces::tree` emits no payloads at all, and `events::detail` caps each payload at `trunc(&raw, 4000)` with the comment "keeps a generous budget" (`crates/render/src/events.rs:108-115`). Line 61 then serializes the *unbudgeted* body and ships it as a peer content item, so the cap is bypassed by construction. There is no size check, no truncation, and no per-request knob — unlike the tool path, where `query_events`/`list_traces` at least carry `limit` + a keyset cursor (`read.rs:202-222`). Pretty-printing adds indentation and escapes payload newlines as `\n`, which inflates tokenization further.
  Note the asymmetry with the tool path: `tools::render_result` puts the raw body in `structuredContent` (`rpc.rs:33-53`), a field a client may keep out of the model's context. Here both items are `contents` — attached resource content is, by definition, injected into the LLM context.
- **Impact**: Markdown for a 30-span trace is ~2–4 KB (~1K tokens). The raw JSON for the same trace with 4K-token prompts and ~500-token completions per span is ~135K+ tokens — roughly **100× the Markdown, and ~99% of the response**, for content the Markdown deliberately omitted. One resource attach can consume most of a 200K window or hard-fail the request; at Opus input rates a single `resources/read` of a fat trace costs on the order of $2. A single `lighttrack://event/{id}` with a large prompt blows past the 4000-char cap the renderer chose.
- **Fix sketch**: Make the JSON item bounded and optional rather than unconditional:
  1. Cap it — serialize compact (not pretty) and, above a `MAX_RESOURCE_JSON` budget (~16–32 KB), replace payload-bearing fields with a marker rather than emitting the blob: walk `spans[].event`, swap `input`/`output` for `"<elided: N bytes — fetch via get_event>"`. This keeps the structural view (ids, models, tokens, cost) that makes the JSON item useful.
  2. Prefer the reverse default: return Markdown only, and add a second template (`lighttrack://trace/{id}?raw=1`, or a `_raw` kind) for the rare structured-consumer case.
  3. Suggested metric: log `markdown_bytes` / `json_bytes` per `resources/read` to stderr; the ratio makes the blowup visible immediately in any real session.
- **Trade-offs**: A client that today parses the JSON item for full payloads loses them at the default. That's the right default for an MCP surface whose consumer is an LLM context — and `get_event` (with its 4000-char budget) remains the payload path. Elision needs a small recursive walk over `spans[].children`, ~30 lines.

## 2. Unknown-shape bodies are pretty-serialized twice and emitted as two identical content items

- **Severity**: Medium
- **Category**: rebuild-per-request / payload-size
- **File**: `crates/mcp/src/resources.rs:59-61`
- **Scenario**: `lighttrack_render::render` returns `None` whenever the value shape is unexpected — e.g. `traces::tree` bails when `trace_id` is empty or the body isn't an object (`crates/render/src/traces.rs:49-53`), which is what an API version skew or a partially-populated record produces. The `unwrap_or_else` fallback then makes `markdown` the pretty JSON, and line 61 makes `raw_json` the *same* pretty JSON.
- **Root cause**: Two independent `serde_json::to_string_pretty(&body)` calls with no shared result. The `unwrap_or_else` is correctly lazy, so the happy path pays once — but the fallback path pays twice, and the two `contents` items are then byte-identical, differing only in `mimeType`.
- **Impact**: Two full deep walks + two full allocations of the whole payload (megabytes for a fat trace) instead of one, and — the bigger cost — the same text is placed in the LLM's context **twice**, doubling the token bill for that read while adding exactly zero information. Precisely the case where the payload is already anomalous and probably large.
- **Fix sketch**: Serialize once and reuse: `let raw_json = serde_json::to_string_pretty(&body).unwrap_or_default();` first, then `let markdown = lighttrack_render::render(render_kind(kind), &body).unwrap_or_else(|| raw_json.clone());`. Better still, on the `None` branch emit a *single* `application/json` content item rather than the same string under two mime types — a duplicate is never the right answer.
- **Trade-offs**: None material. Dropping the duplicate item changes the item count on the fallback path; the spec permits any number of `contents` entries, and no client can be relying on receiving the same bytes twice.

## 3. Error mapping echoes an arbitrary-length upstream body verbatim into the agent's context

- **Severity**: Medium
- **Category**: payload-size / unbounded-result
- **File**: `crates/mcp/src/errors.rs:7-24`
- **Scenario**: The MCP server is designed to run against Cloud Run (`main.rs:5-6`). Infrastructure between the agent and the API — Cloud Run, a load balancer, a proxy — returns HTML error pages, not the API's JSON envelope. `client.rs:77-79` does `resp.text()` with no cap and formats `HTTP {code}: {text}`; `map_error` matches the 5xx arm and emits `format!("error: {g}\n\n{body}")`, planting the whole page in the JSON-RPC error message. Same for any 4xx that isn't a LightTrack JSON error: `format!("error: {body}")` passes it through whole.
- **Root cause**: `map_error` is written for the API's own compact JSON error bodies (which the tests all exercise) and treats "preserve the body verbatim" as unconditional. Nothing bounds the body's length, so the transport-failure path — the one path where the body is *least* likely to be the API's terse JSON — is also the one with no ceiling. Every tool error routes through here (`tools.rs:33,56`), as does `resources/read` (`main.rs:88`).
- **Impact**: A ~10–50 KB Cloud Run/LB HTML page is ~3–15K tokens of markup per failed call, injected into the agent context on the exact turn where the agent is likely to retry and re-inject it. Bounded per call, but multiplied by retries and near-worthless as tokens go — the actionable content is the status code and the guidance line the function already prepends.
- **Fix sketch**: Truncate in `map_error` before formatting: `const MAX_BODY: usize = 2048;` and clip on a char boundary with an explicit marker (`…[truncated, N bytes total]`) so the agent knows content was dropped and doesn't parse the tail as meaningful. Keep the full body on stderr (`eprintln!`) where a human debugging the server can still read it — it costs no context. A 2 KB window preserves every LightTrack JSON error intact (they are far smaller), so no existing behavior or test changes; only non-API bodies clip.
- **Trade-offs**: A genuinely long, genuinely useful API error would be clipped — none exist today (the bodies are `{"error":{"message":"…"}}`), and the marker plus the stderr copy makes the loss recoverable.
