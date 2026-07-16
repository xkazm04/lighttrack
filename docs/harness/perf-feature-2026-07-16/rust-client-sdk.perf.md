# Performance Optimizer — Rust Client SDK

> Total: 3
> Critical: 1 | High: 1 | Medium: 1 | Low: 0

## 1. Unbounded queue + serial worker + blocking `Drop` — the host stalls or OOMs when the collector is slow

- **Severity**: Critical
- **Category**: unbounded-buffer / blocking-caller
- **File**: `clients/rust/src/lib.rs:51-69`, `clients/rust/src/lib.rs:90-94`, `clients/rust/src/lib.rs:157-169`
- **Scenario**: A customer service doing 200 LLM calls/sec. LightTrack's API gets slow (GC pause, deploy, a `spawn_db` queue backing up behind SQLite) and each POST takes the full 2s timeout. The worker drains **one event per round-trip, serially**, so throughput collapses to ~0.5 events/sec while the producer keeps pushing 200/sec into `mpsc::channel()` — which is **unbounded**. The backlog grows by ~199 events/sec, each an owned `serde_json::Value` tree (hundreds of bytes to KBs with `input`/`output` captured). After 10 minutes of degradation the host process is holding ~120k queued `Value`s — tens to hundreds of MB of telemetry the customer never asked to buffer. It never sheds; it only grows until the API recovers or the process dies.
- **Root cause**: Three compounding mechanics.
  (a) `mpsc::channel()` (line 51) is unbounded — `send_raw` (line 92) can never fail-fast or drop, so there is **no backpressure and no shed valve**. The `let _ = tx.send(...)` only errors if the worker is dead, not if it is behind.
  (b) The worker loop (lines 61-67) is strictly sequential: `recv()` → `send()` → block on the response → `recv()`. Drain rate is hard-capped at `1 / RTT`, so any latency increase directly multiplies the backlog.
  (c) `Drop` (lines 162-168) closes the channel and then `h.join()`s — **blocking whatever thread drops the `Client`** until the entire backlog is POSTed one-at-a-time. `flush()` (line 157) is just `drop(self)`, so it inherits this. With 120k queued events at a 2s timeout each, shutdown blocks for ~66 hours; even at a healthy 20ms RTT it is ~40 minutes. Under a container SIGTERM grace period the process gets SIGKILLed mid-drain — so the blocking buys nothing and loses the data anyway. Docs (lines 4-6) promise "non-blocking"; that holds for `send()` but is exactly false for `flush()`/`Drop`.
- **Impact**: Unbounded RSS growth inside the customer's process during any collector degradation — a telemetry SDK causing an OOM-kill of the host app is the worst failure mode an observability vendor has. Plus a shutdown path whose duration is proportional to the backlog and unbounded in the worst case, turning a rolling deploy into a hang. Competitors (Langfuse, Helicone, LangSmith) all cap the buffer and bound the flush.
- **Fix sketch**:
  1. Replace `mpsc::channel()` with a bounded queue (`mpsc::sync_channel(N)`, N ~10k, or `crossbeam::channel::bounded`). In `send_raw`, use `try_send` and on `Full` **drop the event and bump a dropped-counter** — shedding telemetry is always correct versus degrading the host. Expose the counter (`Client::dropped() -> u64` via an `AtomicU64`) so users can see the loss instead of guessing.
  2. Bound the drain in `Drop`: `flush_timeout` (default ~5s, `LIGHTTRACK_FLUSH_TIMEOUT_MS`). Have the worker signal completion via a `Condvar`/`recv_timeout` on a done-channel and `join()` only within the budget; past the deadline, detach the thread and return. `flush()` should return a `bool`/`FlushResult` saying whether it fully drained.
  3. Suggested metrics: queue depth (gauge), dropped-events (counter), worker POST latency + failure rate. Queue depth is the single leading indicator for both the memory and the shutdown risk.
- **Trade-offs**: Bounded queues mean telemetry loss under sustained overload — correct for an SDK, but it must be *visible* (hence the counter), not silent. A bounded flush means some events are lost at exit; that is strictly better than an unbounded hang, and finding #2 shrinks the window enough that the default rarely bites.

## 2. One HTTP round-trip per event — the SDK ignores the batch endpoint that already exists

- **Severity**: High
- **Category**: no-batching / network-efficiency
- **File**: `clients/rust/src/lib.rs:61-67`, `clients/rust/src/lib.rs:84-94`
- **Scenario**: A customer's agent loop emits 8 LLM calls per user request (planner + 6 tool calls + summarizer), at 50 req/s = 400 events/sec. The SDK issues **400 separate HTTPS POSTs/sec**, each with its own request line, `Bearer` header, and ~5ms RTT to a same-region collector. Meanwhile `crates/api/src/events_batch.rs` already accepts a JSON array at `POST /v1/events/batch`, does the whole pipeline per item, and — critically — does **one** store critical section per batch (`insert_events_checked`, events_batch.rs:102-108). The SDK never calls it.
- **Root cause**: `send_raw` enqueues one `(path, body)` pair per event (line 92) and the worker loop `recv()`s exactly one and POSTs it (lines 61-67). There is no accumulation window, no size trigger, no coalescing of same-path items. The `(&'static str, Value)` channel shape is actually well-suited to grouping by path — nothing does it. Each POST therefore pays a full request/response round-trip to amortize a single ~300-byte event, plus a fresh `format!("{base}{path}")` allocation and `bearer_auth` header re-encode per event.
- **Impact**: Two-sided cost. **Client-side**: the worker's max drain rate is `1/RTT` — at 5ms that is 200 events/sec, already below this workload's 400/sec, so the queue from finding #1 grows *in steady state, on a healthy collector*. Batching 100 events per POST raises the ceiling ~100× to ~20k events/sec and cuts per-event syscall/TLS-record overhead proportionally. **Server-side**: 400 req/s of auth + `spawn_db` hops collapse to ~4 req/s of batches, and 400 store critical sections become 4 — that is the difference between the collector being the bottleneck and it being free. Header/TLS-framing overhead per event (~200+ bytes of HTTP framing on a ~300-byte payload) roughly halves the bytes on the wire.
- **Fix sketch**: Give the worker a batching loop instead of a bare `recv()`:
  ```
  loop {
      let first = rx.recv() else break;
      let mut batch: HashMap<&'static str, Vec<Value>> = ...;
      // accumulate until MAX_BATCH (e.g. 100) or FLUSH_INTERVAL (e.g. 250ms), whichever first
      let deadline = Instant::now() + flush_interval;
      while batch_len < max_batch {
          match rx.recv_timeout(deadline - Instant::now()) { Ok(item) => push, Err(_) => break }
      }
      // POST each path's Vec once
  }
  ```
  Route `/v1/events` → `/v1/events/batch` with a JSON array when `len > 1`; keep the single-event path for `len == 1` so latency-sensitive lone events don't wait. Respect the server's `LIGHTTRACK_MAX_BATCH` cap (events_batch.rs:66-72) — chunk at the client's `max_batch` and keep it at or below the server default. `/v1/scores` has no batch endpoint today, so leave it single until one exists. Note the batch endpoint returns per-item multi-status under a 200; the SDK is fire-and-forget so it can ignore the body, but counting `rejected`/`invalid` into a counter would give users their first real ingest-error signal.
- **Trade-offs**: Adds up to `flush_interval` (~250ms) of delivery latency per event — irrelevant for observability data, and the caller is already async so it costs the host nothing. Slightly larger worst-case in-flight memory (one batch buffered), bounded by `max_batch`. A failed batch loses N events instead of 1, which argues for keeping `max_batch` at ~100 rather than 1000.

## 3. `guard()` recompiles every regex on every call, inline on the caller's request path

- **Severity**: Medium
- **Category**: allocation / blocking-caller
- **File**: `clients/rust/src/lib.rs:361-386` (and `clients/rust/src/lib.rs:99-124` for the `track_guard` entry point)
- **Scenario**: A customer validates every model output with `GuardRules { no_pii: true, must_not_match: vec![banned1, banned2], .. }` — exactly the shape the quickstart demonstrates (`quickstart.rs:36-37`). Unlike the event path, `guard()` runs **synchronously in the caller's request path** by design (it must, since the caller branches on the verdict). At 200 guarded outputs/sec, `regex::Regex::new` is invoked 6× per call = 1200 regex compilations/sec, every one of them recompiling a pattern that never changed.
- **Root cause**: Every regex is constructed at its use site: `must_match` (line 362), each `must_not_match` (line 369), and each of the 4 `PII_PATTERNS` (line 376) — inside a per-call loop, on every invocation. `regex::Regex::new` parses the pattern into an AST/HIR and builds an NFA/DFA program; it is deliberately expensive (tens of µs, and heap-allocation-heavy) precisely because the crate expects it to be hoisted. The `PII_PATTERNS` set is a compile-time constant (lines 306-311) — there is no reason for it to be rebuilt ever. The `GuardRules`-supplied patterns are per-rules-struct, not per-call, and `GuardRules` is `Clone` and typically constructed once and reused. This is the well-known regex anti-pattern the crate's own docs warn against ("compiling in a loop").
- **Impact**: ~50-150µs of pure CPU per guarded call, all of it on the customer's request thread, all of it garbage. At 200/sec that is a few percent of one core burned on rebuilding identical automata, plus the allocator churn (each compile allocates and immediately frees a whole program). It is bounded and small relative to a 500ms LLM call — hence Medium, not High — but it is a tax the SDK charges on a path that advertises itself as "deterministic, network-free" and cheap. It also scales with `must_not_match.len()`, so a customer with a 20-pattern banned list pays ~20× and gets a genuinely noticeable inline cost.
- **Fix sketch**: Two independent hoists, both mechanical.
  1. **PII (free, no API change)**: make the compiled set a lazy static — `static PII_RES: OnceLock<Vec<(&'static str, Regex)>>` compiled once on first use, iterated thereafter. Compile-time-constant patterns should never touch `Regex::new` at runtime.
  2. **User patterns**: add a compiled representation cached on the rules. Cheapest non-breaking version: a `CompiledGuardRules` (or a `OnceLock<Vec<Regex>>` field inside `GuardRules`) built once via `GuardRules::compile()`, with `guard()` taking the compiled form and the current `guard(&str, &GuardRules)` kept as a thin wrapper that compiles-then-calls, so existing callers keep working. This also surfaces invalid patterns at construction rather than silently turning them into a per-call violation (lines 364, 370) — today a typo'd regex fails every single call *and* pays the failed-compile cost each time.
  Suggested metric: none needed — this is verifiable directly with a criterion bench over `guard()` before/after; expect the PII-only path to drop from ~50µs to well under 5µs.
- **Trade-offs**: Adding a compiled cache to `GuardRules` costs it its trivial `Clone`/`Default` derive ergonomics (a `OnceLock` field is not `Clone`; use a `Vec<Regex>` in a separate compiled type, or `Arc` it). Keeping the existing `guard(&str, &GuardRules)` signature as a wrapper means the naive caller sees no improvement on user-supplied patterns — but they get the PII hoist for free, which is the common case the quickstart teaches.

---

### Checked and deliberately not filed

- **Double serialization** (`track` at lib.rs:86 does `to_value(&ev)` to build a full `Value` tree, which the worker then re-serializes to bytes via `.json(&body)`). Real waste — one intermediate allocation tree per event — but ~1-2µs on a background thread, off the caller's path, and dwarfed by findings #1/#2. Would become worth doing as part of the #2 batching rework (send `Vec<LlmEvent>` and serialize once), not on its own.
- **Thread-per-`Client`** (lib.rs:52): a `Client` is a per-process singleton in every documented usage; one extra OS thread is not a finding.
- **No retry / 2s fixed timeout** (lib.rs:56, 66): a reliability and data-loss question, not a performance one — and `let _ = req.send()` correctly refuses to propagate failure into the host.
- **`format!("{base}{path}")` per POST** (lib.rs:62): a genuine per-event allocation, but a single small `String` — textbook negligible micro-optimization, and finding #2 amortizes it ~100× anyway.
