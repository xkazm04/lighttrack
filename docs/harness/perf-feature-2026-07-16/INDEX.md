# Performance Optimizer + Feature Scout Scan — LightTrack, 2026-07-16

> Dual-lens audit (`perf_optimizer` + `feature_scout`), 3 findings per lens per context.
> 30 contexts × 2 lenses = 60 parallel subagent runs, batched in waves of ≤10.
> Ran against a **rebuilt context map** (18→35 contexts, 120→200 files, 60%→100% coverage) —
> the rebuild made 7 whole subsystems visible for the first time (responder, device agent,
> collective intelligence, forecasting, prompt registry, trace trees, margin/FX).

---

## Totals

| | Critical | High | Medium | Low | **Total** |
|---|---:|---:|---:|---:|---:|
| Across 30 contexts | 18 | 103 | 58 | 0 | **179** |
| Share | 10% | 58% | 32% | 0% | 100% |

Split by lens: **Performance ≈ 90 findings** (all 18 criticals are perf-or-money), **Feature ≈ 89 findings**.
Verified two ways: 179 `## N.` finding headings = 179 `**Severity**:` bullets.

---

## The 18 Critical findings — one-line summary, grouped by theme

### A. The reactive loop bills without a governor (responder + agent + judge)
1. **Auto-Responder — investigate stage completely ungated** — no concurrency cap, cooldown, or dedup; the `Breaker` guards only ACT and counts only *applied* fixes, so a flapping project spawns one `claude` session per alert (~$1 each, ~$60/hr/project, self-amplifying). `crates/responder/src/investigate.rs:23`, `breaker.rs:23`
2. **Auto-Responder — webhook forks a billable child per POST** — same ungated spawn seen from the entry point; unauthenticated endpoint, spend bounded only by the *remote* alerter's cooldown. `crates/responder/src/webhook.rs:59`
3. **Device Agent — Command connector deadlocks the whole agent on a >64KB envelope** — writes full stdin before draining stderr, no `wait_with_output` timeout; the cloud's lease expiry then re-runs the paid Claude call forever. `crates/agent/src/connect.rs:50`
4. **Pairwise judge cost is quadratic in targets with no cap/estimate/confirmation** — 8 targets × 100 cases = 5,600 judge calls (~$56) before the cost is even printed; `let o = outcome?` discards every paid verdict and skips `post_run` on one tail failure. `crates/runner/src/pairwise.rs:115`
5. **Score runner — client-side anti-join hard-capped at 1000 rows** — past 1000 scores, idempotency silently breaks and events get re-judged, burning credits + up to 1000 billed Firestore reads per `--interval` tick. `crates/runner/src/score.rs:59`

### B. Money-truth: the headline numbers are wrong or unenforced
6. **Firestore `cost_by_dimension` full-scans the entire events collection per margin request** — ~$3 in billed doc reads/request at 5M events, full in-RAM materialization. `crates/store-firestore/src/revenue.rs:49`
7. **Firestore admission check reads the entire rolling window on every ingest** — O(N²) billed reads over a window, ~$0.30/event at 1M in-window events. `crates/store-firestore/src/events.rs:58`
8. **`daily_cost_by_dimension` full-scans the whole events table per forecast request** — non-sargable `(?1 IS NULL OR project_id=?1)` defeats `idx_events_project_ts`, under the global mutex, backpressuring ingest. `crates/store/src/sqlite/forecast.rs:53`
9. **Per-project redaction policy is decorative — stored, displayed in a "Redaction" column, never enforced** — real redaction is env-only; `Hash`/`Drop` variants unimplemented, so a project shown as `drop` still stores raw payloads. A false privacy claim on the trust boundary. `crates/core/src/project.rs:4`

### C. Ingest hardening that doesn't harden
10. **Ingest has no idempotency key** — `idempotency.rs` serves only webhook dedup; a retried write after timeout maps to 409/`invalid`, indistinguishable from malformed. `crates/api/src/idempotency.rs`
11. **A "batch" is 500 autocommit transactions + 500 limit-rule reloads under one held connection** — the efficient path is worse than 500 single posts. `crates/api/src/events_batch.rs:101`

### D. Structurally dead / silently-degraded features
12. **Served prompt versions are never attributed to traffic** — `ResolvedPrompt` discards the resolution, `LlmEvent` has no prompt/version key; "did v4 beat v3?" is unanswerable though cost + scores both exist. `crates/api/src/prompts.rs:179`
13. **Span tree renders anonymous, undebuggable nodes** — `render_node` drops `event.name`/`error`/`id` though all are in the payload; the trace is a cost report, not a debugger. `crates/render/src/traces.rs:115`
14. **The CI gate is blind to judge trust** — `bench --gate` branches only on score status; the `lt:calibration:` trust rubric has no reader outside `calibrate_watch.rs`, so an untrusted judge ships green. `crates/runner/src/gate.rs:6`

### E. Collective intelligence: pooled without consent, served without k-anonymity
15. **Contribution is all-or-nothing** — `gather_run_stats` walks every project/run unconditionally; no per-project opt-in, no consent record, withdrawal only via POSTing an empty digest. `crates/api/src/collective.rs:128`
16. **`/v1/collective/leaderboard` loads the entire `collective_entries` table per request; filters applied post-merge in Rust** — the `idx_collective_model` index is used by no query. `crates/api/src/collective.rs:269`

### F. Client & MCP surfaces that harm their host
17. **SDK: unbounded queue + serial worker + blocking `Drop`** — a slow collector grows the *customer's* RSS without bound and turns their shutdown into an unbounded hang, despite docs promising "non-blocking". `clients/rust/src/lib.rs:51`
18. **MCP `resources/read` attaches full untruncated raw JSON alongside the Markdown** — ~135K tokens (~100×) for a 30-span trace, bypassing the 4000-char cap `events::detail` enforces. `crates/mcp/src/resources.rs:58`

---

## Cross-cutting themes (the real fix leverage)

These recurred across many contexts and independent subagents — fixing the root cause closes a cluster of findings at once.

| # | Theme | Where it recurs | Root cause / one-fix leverage |
|---|---|---|---|
| T1 | **Reactive loop bills without a governor** | responder (×3), agent, pairwise, score, benchmark-resume | No shared cap/cooldown/dedup/estimate before spawning a paid `claude`/judge call. One admission-control primitive covers most. |
| T2 | **Backend-parity: SQLite works, Postgres/Firestore silently wrong** | events-query, budget-limits, forecast, margin, billing-Polar, collective | ~20 `Store` trait methods have plausible-but-wrong default impls the cloud backends inherit. **Root cause: the conformance suite never calls those methods** (`store/src/conformance.rs:23`) — one test-coverage fix would have caught the whole class. |
| T3 | **Stored-but-never-read fields (dead capabilities)** | redaction policy, `effective_date`, FX `converted`, `last_used_at`, `Customer`/`BillingProduct`, calibration `bias`/`trusted`, `retry_after_secs` | A field is persisted/serialized/rendered but has zero consumers — the promised capability is structurally dead, often a false claim (privacy, audit, reproducibility). |
| T4 | **`UsageCache` under-wired → read paths recompute full scans** | budget-status, forecast, margin, limits | The incremental cache is reachable only from `insert_event_checked`; every read path re-runs a rolling-window `SUM/COUNT` under the global mutex, so dashboard polling stalls ingest admission. |
| T5 | **Serial-outer / parallel-inner and per-item round-trips** | trace-score, pairwise, rubric-score, dataset-build, benchmark-compare, ingest-batch, billing-sync | Parallel judging bottlenecked by a serial store write; or one HTTP request per item with a redundant per-item auth/config reload. Firestore transport root cause: `commit_update` hard-codes a 1-element write array (`store-firestore/src/rest.rs`). |
| T6 | **Fixed-interval busy-polls on the global SQLite mutex** | relay lease, job queue, calibration watch | 1–5s poll loops that write per tick, contending with ingest; no event-wake/backoff. ~112k txns/day/worker on the relay alone. |
| T7 | **Silent 1000-row / limit caps break correctness** | score anti-join, relay dead-letter, calibration drift-window, CLI `--limit` | A pagination cap silently truncates a set used for a correctness decision (dedup, sweep, drift) → re-judging, missed sweeps, unreachable drift level. |
| T8 | **Reproducibility broken at three layers** | judge-engine (no temp/seed), prompt-registry (versions unattributed), rubric (no version, id discarded) | An eval product's core promise; each layer independently prevents reproducing a score. |
| T9 | **Auth hot-path: uncached round-trip + write-amplification** | projects-access-control, platform-core (2 audits agree) | Every authenticated request takes the global mutex twice — key SELECT + unconditional `touch_api_key` UPDATE — with no positive/negative cache. |
| T10 | **Doc/README claims contradicted by code** | responder "auto-fix out of scope" (it's wired), RELAY.md sweep (unreachable) + backoff field (unpopulatable), redaction docstring, "versioned dataset" | Stale or aspirational docs that the code does not honor. |

---

## Per-context breakdown

(Sorted by criticals desc, then total. P = perf lens, F = feature lens.)

| Context | Group | P (C/H/M) | F (C/H/M) |
|---|---|---|---|
| Auto-Responder Pipeline | Integration | 1 / 1 / 1 | 0 / 2 / 1 |
| Responder Investigation & Actions | Integration | 1 / 1 / 1 | 0 / 2 / 1 |
| Device Agent (lt-agent) | Integration | 1 / 1 / 1 | 0 / 2 / 1 |
| Collective API & Rendering | Integration | 1 / 1 / 1 | 1 / 2 / 0 |
| Event Ingestion & Query | Trace/Privacy | 1 / 2 / 0 | 0 / 2 / 1 |
| Ingest Hardening & Idempotency | Trace/Privacy | 1 / 1 / 1 | 1 / 1 / 1 |
| Cost Forecasting | Cost | 1 / 1 / 1 | 0 / 2 / 1 |
| Margin Simulation & FX | Cost | 1 / 2 / 0 | 0 / 2 / 1 |
| Projects & Access Control | Trace/Privacy | 0 / 2 / 1 | 1 / 1 / 1 |
| Prompt Registry & Versioning | Judge | 0 / 2 / 1 | 1 / 2 / 0 |
| Trace Trees & Span Scoring | Trace/Privacy | 0 / 2 / 1 | 1 / 1 / 1 |
| Judge Pairwise/Parse/Pool | Judge | 1 / 1 / 1 | 0 / 2 / 1 |
| Score Recording & Query | Judge | 1 / 1 / 1 | 0 / 2 / 1 |
| Calibration Watch & Run Stats | Bench | 0 / 2 / 1 | 1 / 2 / 0 |
| MCP Resources & Error Mapping | Integration | 1 / 0 / 2 | 0 / 2 / 1 |
| Rust Client SDK | Integration | 1 / 1 / 1 | 0 / 2 / 1 |
| Relay Task Leasing (cloud) | Integration | 0 / 2 / 1 | 0 / 2 / 1 |
| Collective Intelligence Core | Integration | 0 / 2 / 1 | 0 / 2 / 1 |
| Budget Limits & Breach Alerts | Trace/Privacy | 0 / 2 / 1 | 0 / 2 / 1 |
| Alert Attribution & Channels | Trace/Privacy | 0 / 2 / 1 | 0 / 2 / 1 |
| Model Pricing & Cost Rollup | Cost | 0 / 1 / 1 | 0 / 2 / 1 |
| Revenue & Margin Tracking | Cost | 0 / 2 / 1 | 0 / 2 / 1 |
| Billing Integrations (Stripe/Polar) | Cost | 0 / 1 / 1 | 0 / 2 / 1 |
| Judge Engine | Judge | 0 / 2 / 1 | 0 / 2 / 1 |
| Scoring Rubrics | Judge | 0 / 1 / 1 | 0 / 2 / 1 |
| Evaluation Datasets | Bench | 0 / 2 / 1 | 0 / 2 / 1 |
| Benchmark Suites & Runs | Bench | 0 / 2 / 1 | 0 / 2 / 1 |
| Background Job Queue | Bench | 0 / 1 / 2 | 0 / 2 / 1 |
| Platform Core & API Server | Platform | 0 / 2 / 1 | 0 / 2 / 1 |
| Cloud Store Backends (PG+FS) | Platform | 0 / 2 / 1 | — |
| Store Trait, SQLite & Plumbing | Platform | — | 0 / 2 / 1 |
| Markdown Render & Operator CLI | Integration/Platform | 0 / 1 / 1 | — |

(Store-trait + codec share one feature report; markdown-render + CLI share one perf report — hence 61 report files across 30 context units.)

---

## Suggested fix-wave split

Ordered by risk × leverage. Each wave shares one mental model so fixes compound.

- **Wave 1 — Stop the bleeding (T1): admission control on paid calls.** Responder investigate/webhook gating, agent connector timeout+deadlock, pairwise/score/benchmark caps + confirmation + don't-discard-paid-verdicts. Closes 5 criticals.
- **Wave 2 — Money-truth Firestore/forecast scans (T2/T4 perf).** Sargable/aggregation queries for `cost_by_dimension`, `usage_since`, `daily_cost_by_dimension`; wire `UsageCache` into read paths. Closes criticals #6/#7/#8 + the budget/margin/forecast highs.
- **Wave 3 — Privacy & consent integrity.** Enforce (or honestly remove) per-project redaction; collective opt-in + consent record + k-anonymity over sources; redaction audit trail + `error`/`tags` scrubbing. Closes criticals #9/#15/#16.
- **Wave 4 — Backend parity + conformance (T2 root cause).** Extend `conformance.rs` to exercise the ~20 default-bearing methods; implement the filtered/windowed/scoped/tokens methods on PG + Firestore; batch the Firestore transport (`commit_update`). Turns a class of silent prod bugs green→honest.
- **Wave 5 — Ingest correctness (T3/C).** Real ingest idempotency key; batch = one transaction + hoisted limit read; partial-success semantics + machine-readable rejections.
- **Wave 6 — Eval reproducibility (T8).** temperature/seed on all providers; prompt-version attribution on events; rubric versioning + id capture.
- **Wave 7 — Dead-capability sweep (T3) + operability.** Wire or remove `effective_date`, FX `converted`, `Customer`/`BillingProduct`, calibration `bias/trusted`, `retry_after_secs`; store-exercising `/health`, `/metrics`, graceful shutdown.
- **Wave 8+ — Feature build-out.** Trace debugger fields, run-vs-run diff, human relabel of scores, dataset golden-set annotation, Polar backfill + refund reconciliation, span-tree from the Rust SDK, prompt/dataset/rubric MCP resources.

---

## How this scan was run

- **Scanner prompts:** Vibeman registry `agent_perf_optimizer` + `agent_feature_scout` (`src/lib/prompts/registry/agents/`), adapted per-context with Rust/axum framing, competitor context (LangSmith/Langfuse/Helicone/Braintrust), and an explicit **anti-padding + verify-before-file** instruction ("2 real findings beat 3; report what you verified, not what you'd expect").
- **Context map:** rebuilt first (Phase B pre-flight) — the shipped map covered 120/200 Rust files (60%) and was blind to the `responder` and `agent` crates entirely. Added 17 context units, fixed 1 ghost, reached 200/200. All 18 criticals sit in code the *old* map could not see or scan at full coverage.
- **Scope:** all 30 context units, both lenses, backend + client (`crates/` + `clients/`).
- **Method:** one `general-purpose` subagent per (context × lens), each read all in-scope files (typically 6–12 including neighbours), wrote one structured report, replied with terse stats. Orchestrator read only replies during scanning.
- **Baseline (unchanged by this read-only scan):** `cargo check --workspace --all-targets` clean; 222 tests passing, 0 failed.
- **Quality signal:** subagents routinely reported **fewer than 3** findings when the third didn't survive scrutiny (billing-perf, scoring-rubrics-perf, model-pricing-perf, alert-attribution, render/CLI), and each documented candidates checked-and-dropped with reasons — several disproved hypotheses the orchestrator had primed (FX-per-request, missing api_keys index, JSON-schema rebuild, price-book re-read, HTTP client rebuild). Convergent findings from independent subagents (auth hot-path, redaction theater, `UsageCache` bypass) are noted in the cross-cutting themes.
