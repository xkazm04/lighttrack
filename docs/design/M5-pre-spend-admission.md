# M5 — Pre-spend admission: SDK-side `admit()` first, relay enqueue admission, gateway later

Size L (this wave) → XL (gateway, deferred) · gate policy · wave D · contexts: client-sdks,
api-server, limit-enforcement, relay-queue, cost-pricing · depends on M30 (fixtures), M4 (basis on
status), M7 (relay enqueue path)

## Problem
Every cap LightTrack has is record-side: it refuses to *record* a call that already cost money. The
API already tells every SDK how close it is to a cap — `usage_ratio` and `shed_fraction` on accepted
writes (`crates/api/src/events.rs`), `Retry-After` on 429s (`error.rs`) — and all three SDKs discard
it (`clients/typescript/src/index.ts` `if (resp.ok) return;`, `clients/python/lighttrack/client.py`
`with urlopen(req): pass`, `clients/rust/src/lib.rs` `Ok(resp) if resp.status().is_success() => {}`);
M30 added `parseLimitView`/`parse_limit_view` but nothing consumes them. `docs/ARCHITECTURE.md` §4
defers the inline gateway because it "adds latency and a critical-path dependency" — an SDK-side
admission loop has neither. Separately, relay `enqueue_task` performs zero limit checks and prices
runs at a flat $1 (`crates/api/src/relay.rs`), contradicting D0 (headless `claude -p` meters at API
rates); the device now reports `cost_usd` (M6) and the cloud ignores it.

## Design
### A. SDK admission (all three SDKs, fixture-driven)
1. API (small, additive): response headers `X-LightTrack-Usage-Ratio`, `X-LightTrack-Shed-Fraction`,
   `X-LightTrack-Retry-After` mirroring the body on `/v1/events`, `/v1/events/batch`, `/v1/traces`
   (OTLP door); `IngestResponse += binding_scope: Option<{kind, value}>` naming the scope of the
   worst rule so a name-scoped cap can be cached per name. Document in ARCHITECTURE §7c.
2. Each SDK keeps a `LimitView { usage_ratio, shed_fraction, retry_after_until, binding_scope, refreshed_at }`
   per (project, key[, scope]) updated from every ingest response and 429; exposes
   `admit(name?) -> Admit { ok, reason, retry_after_secs }` (pure decision, no I/O); `enforce:
   "block" | "warn" | "off"` (default `off`) on `wrapOpenAI`/`instrument`/Rust `Client` that
   short-circuits the provider call with a typed `LightTrackBudgetExceeded` error carrying
   `retry_after`. Optional refresh from `GET /v1/limits/status?project=` with a bounded TTL
   (default 30 s) when the view is stale and `enforce != off`. A locally blocked call is **not**
   recorded as spend; with `record_blocked: true` emit a zero-usage event tagged
   `lt_blocked_locally` so rollups stay honest. Shed decision uses the same hash as the server
   (ARCHITECTURE §7c) so client and server agree which events shed.
3. Fixtures: extend `clients/contract/fixtures/limits.json` with admission cases (429 +
   `Retry-After: 30` refuses for 30 s; `usage_ratio >= 1.0` refuses; shed fraction applies
   deterministically); all three runners assert them. Manifests flip `admit` to `supported`;
   regenerate `clients/README.md` (`node scripts/gen-sdk-matrix.mjs`).
### B. Relay: price from the envelope and admit at enqueue
4. `relay_run_event`: `cost_usd = req.cost_usd (M6 envelope) ?? PriceBook by (anthropic, model, tokens) ?? relay_flat_cost`,
   stamping `metadata.cost_source ∈ {envelope, book, flat}`; `LIGHTTRACK_RELAY_FLAT_COST_USD`
   becomes the last resort. Settle-time event stays un-admitted (the run happened).
5. `enqueue_task`: pre-check the project's limit status (reuse `events_admission`'s evaluation in a
   read-only mode; M4's `basis` appears in the message); hard breach → 429 with the breach reason
   (admission verdict shape from M18 if merged: `refused`); soft → `warning` on the returned task.
6. `tests_relay.rs`: the flat-cost test becomes a three-way `cost_source` test; an
   over-budget enqueue → 429 test. `docs/RELAY.md` "Cost model" rewritten; `docs/DECISIONS.md`
   D18 "Relay runs are metered traffic; enqueue is the admission point" (supersedes the $1 premise;
   does not touch D4 — relay is monitored traffic, not the judge).
### C. Gateway (NOT this wave)
Record the `lt-gateway` design as a follow-on in `docs/design/README.md` (one paragraph) — it needs
its own DECISIONS entry and is out of scope here.

## Gates
Rust: `cargo build/test/clippy` for lighttrack-api, -core; `cargo test -p lighttrack-client` in
`clients/rust`; TS `npm test`; Python `python -m pytest clients/python/tests`; matrix generator
`--check`. Never build the whole workspace.

## Evaluation
Before: 3/3 SDKs ignore proximity signals; relay cost 100% flat; enqueue does 0 limit checks.
After: fixture suite asserts refusal after 429 in three languages; `metadata.cost_source` present on
every relay event with `flat` only as fallback; over-budget enqueue → 429.
