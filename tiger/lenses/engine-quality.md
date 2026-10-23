---
note_type: lens
lens: engine-quality
updated: 2026-09-15
source: Tiger v2.3.0 references/lenses.md
---
# Lens A - Engine Quality

This lens audits the code around the model. It makes no Character or model call and is fully L1.
Each engine note records three dials. Init values are inventory estimates only.

## Dial 1 - Wrapping / chokepoint (N/10)

Is the call defended, and defended in one place?

- [ ] One provider-switching wrapper every call traverses; retries, fallback, cost stamping, and telemetry are not scattered beside raw SDK calls.
- [ ] Provider abstraction permits model/vendor changes without editing business call sites.
- [ ] Retry and failover occur before hard failure.
- [ ] There is a per-attempt timeout and a real total wall-clock budget across attempts.
- [ ] Abort/cancellation propagates when the requesting client disconnects.
- [ ] Structured output is requested; a typed schema is normalized and validated; one bounded self-repair is available; the decoder never turns malformed output into truth.
- [ ] A parseable-but-empty reply cannot succeed.
- [ ] Input fields, arrays, output, and durable rows have explicit bounds.
- [ ] Rate limit and quota are scoped to the caller or tenant where appropriate.
- [ ] Temperature and maximum output are appropriate to the task; measurement paths request deterministic sampling.
- [ ] Degradation reaches a deterministic floor only when it is visibly flagged.
- [ ] Calls carry a task/use-case tag sufficient for attribution.

### LightTrack levers

- HTTP model traffic converges on lighttrack_engine::providers::generate_chat at
  crates/engine/src/providers.rs:213-237 and generate_once at :299-370. Claude process traffic
  converges on lighttrack_engine::invocation::run at crates/engine/src/invocation/run.rs:20-102.
- Provider requests have 10-second connect and 120/900-second per-attempt bounds at
  crates/engine/src/providers.rs:36-76. The retry ladder declares a 60-second budget at
  crates/engine/src/retry.rs:10-22,86-127; a run must verify whether time spent inside an attempt
  can exceed that nominal total because the deadline is checked between attempts.
- Structured judge families use provider schema requests, typed parsing, local clamps, and one
  repair at crates/engine/src/judge.rs:81-121,144-187 and crates/engine/src/parse.rs:111-159.
- Untrusted judge content uses a fresh nonce boundary and collision signal at
  crates/engine/src/fence.rs:28-116. Responder prompt text uses fixed textual fences and explicit
  untrusted-data instructions; that is a different enforcement strength to test.
- Gateway admission is pre-spend at crates/gateway/src/chat.rs:48-53 and tool incompatibility
  fails before generation at :34-46. Cancellation on caller disconnect and uniform request-size
  limits require explicit verification.
- Responder edit is default-off, clean-tree gated, branch isolated, circuit-broken, and tested at
  crates/responder/src/act.rs:40-141. Device posture is resolved before spend at
  crates/agent/src/exec.rs:41-77.

## Dial 2 - Observability (N/10)

Can an operator debug a bad answer and bill it honestly?

- [ ] Token usage is metered and committed only under the product's declared usable-attempt semantics.
- [ ] Latency is captured.
- [ ] Every attempt outcome is observable, not failures alone.
- [ ] Prompt and raw response can be captured for post-hoc evaluation without silently collecting sensitive content.
- [ ] Request/trace id, model, tokens, cost, attempts, repair, and degraded/demo state are attributable.
- [ ] Captured prompt/log data has secret and PII redaction.
- [ ] A golden/eval harness fingerprints prompt plus schema and accumulates prompt to output to verdict evidence.
- [ ] Every route to a deterministic/degraded floor discloses equally in the immediate user surface and durable artifact.

### LightTrack levers

- Judge outcomes retain model, cost, latency, tokens, determinism, injection suspicion, sample
  coverage, agreement, parse failure, and batch provenance at crates/engine/src/lib.rs:258-353.
- Repair accounting includes both provider calls at crates/engine/src/parse.rs:63-108.
- Gateway response provenance includes route, served target, fallback, attempts, determinism, schema,
  usage, and cost at crates/gateway/src/chat.rs:136-186; telemetry parity is a run-time check.
- HTTP targets preserve unknown cost/usage as absent and measure wall time when the target omits it
  at crates/engine/src/http_target.rs:132-155.
- Device actions compute rendered prompt SHA-256 and make full prompt/result reporting opt-in at
  crates/agent/src/actions.rs:54-69 and crates/agent/src/report.rs:87-113.
- Responder's narrow ClaudeRun retains text/model/cost/success but drops token and latency fields at
  crates/responder/src/invoke.rs:14-21,56-82. Whether this blocks a Character job is assessed only
  during a run.

## Dial 3 - Caching / efficiency (N/10)

Are identical tokens and results paid for twice?

- [ ] Result caching uses a stable correctness key and declared lifetime.
- [ ] Stable system/context prefixes use provider prompt caching where supported.
- [ ] Identical concurrent calls are deduplicated in flight.
- [ ] Context has one global budget; stable and per-request layers are deliberate; windows retain the highest-signal evidence and disclose omissions.
- [ ] Metering recognizes cached input and applies the app's cached-input price.

Every confirmed gap must include a cost implication per call and per month at measured volume.

### LightTrack levers

- The repository price book has cached-input rates at config/pricing.json:15-22 and engine outcomes
  distinguish input/output/reasoning tokens, but init found no general result cache, provider prefix
  cache annotation, or in-flight identical-call dedup in the model dispatch.
- Batched rubric judging is an explicit transport optimization that amortizes repeated context and
  records batch_size at crates/engine/src/judge/batch.rs:1-18,148-195.
- Responder admission cooldowns, hourly limits, and concurrency caps prevent repeated alert spend at
  crates/responder/src/config.rs:43-55,145-160. They are admission controls, not semantic result caches.
- Context-length and list-count bounds differ by surface. Each engine note names the canonical
  grounding set that later runs should measure before proposing a cache or truncation change.

## Verdict contract

Findings carry lens: engine-quality, no Character, a dimension in observability, cost, or trust,
and code_check in confirmed-absent, present-but-missed, present-broken, by-design, or n-a.
Strengths are first-class and identify machinery that must not be refactored away. Every use-case
hard check in [[../README]] is audited under this lens and tagged to its use case.
