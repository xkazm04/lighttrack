# Follow-ups — perf+feature campaign 2026-07-16

## ✅ DONE (Wave 6): eval reproducibility

- judge-engine #1 (High) `bb57d79` — deterministic judge sampling (temp 0 + seed).
- prompt-registry #2 + benchmark-suites #3 `b46a92f` — version-aware promotion gate + run-report pins.
- prompt-registry #1 (Critical) `a832674` — prompt-version attribution + `GET /v1/costs/prompts`.

## ✅ DONE (Wave 7): dead-capability — API key lifecycle

- projects-access-control #2 (High) `87784d5` — `list_api_keys` + `set_api_key_revoked` (3 backends,
  conformance-pinned) + `GET/DELETE /v1/projects/:id/keys[/:kid]`. Wires `last_used_at` + `revoked`.

## ⚠ OPEN — remaining theme T3 (dead capability) + operability

- **Project mutation (projects-access-control #3, Med):** `PATCH /v1/projects/:id` + `update_project`;
  the gate is *enforcing* `enabled` on ingest, which must extend the Wave-3 policy cache (enforce-or-
  nothing — a settable-but-ignored `enabled` recreates the anti-pattern). Fold the cache to carry
  `(enabled, redaction)` per project.
- **FX `converted` lossy** (margin-sim #2): persist original minor amount + rate on `RevenueEvent`
  (schema, 3 backends, live-DB verify) so a wrong conversion is correctable.
- **`effective_date` unread / price book overwrites** (model-pricing #H2): effective-dated pricing.
- **`Customer`/`BillingProduct` dead structs** (revenue-margin #1): the "Phase 2 sync" — product call.
- **calibration `bias`/`trusted` unconsumed** (judge-engine): wire trust into the gate.
- **agent `retry_after_secs` + `paused_until` loop** (device-agent #1): rate-limit backoff, own session.
- Operability: store-exercising `/health`, `/metrics`, graceful shutdown (platform-core feature).

## ✅ DONE (Wave 5 session): ingest correctness

- ingest-hardening perf #1 (Critical) closed `d445bc4` — batch = one transaction + hoisted limit rules.
- ingest-hardening feature #1 (Critical) + #3 + perf #2 closed `1bec836` — duplicate-id = replay
  (PK-backstop, requires client id+ts — shipped SDKs send both), batch items carry index/id/code,
  double deep-clone killed.
- ingest-hardening perf #3 (Medium) closed `221b847` — rejection-ledger prune amortized (60s gate).

## ⚠ OPEN (wave-5 tail)

- **Drop ledger / ingest health (ingest-hardening feature #2, High):** generalize `RejectionLedger`
  to a `DropReason`-keyed ledger (LimitBreach | InvalidModel | TsSkew | UnresolvedProject |
  DuplicateConflict | BatchTooLarge | BodyTooLarge) + `GET /v1/ingest/health` with per-bucket counts,
  est. missed cost, and one allowlisted sample detail. The "why are my events missing?" answer;
  self-contained DX feature.
- **`Idempotency-Key` envelope fast path:** generalize `SeenWebhooks` → `SeenKeys<V>` caching the
  serialized `BatchResponse` per `(project, key)`; the PK replay (shipped) stays the durable backstop.
  Then give the Rust SDK a retry-on-5xx/timeout loop keyed per batch and stop discarding responses.

## ✅ DONE (Wave 3 session): privacy & consent integrity

- projects-access-control #1 (Critical) closed `039bdae` — per-project redaction (hash/drop) enforced
  on ingest via an AppState policy cache; env PII scrub now also covers `error`/`tags`.
- collective-api-rendering #1 (Critical) closed `fef1c68` — `collective_opt_in` consent flag (default
  off) across core + schemas + 3 backends; digest walks only consenting projects + consent envelope.
- collective-api-rendering #2 (High) closed `454131b` — `min_contributors` source floor (default 2),
  applied before filters, suppression disclosed as `held_back`.
- ⚠ Release-note flips: hubs contribute nothing until projects opt in; single-contributor hubs show an
  empty board at the default floor (`held_back` explains).

## ⚠ OPEN (wave-3 tail): collective serving + consent UX follow-ons

- **collective-api-rendering.perf #1 (Critical, bounded)** — leaderboard decodes the full (capped)
  `collective_entries` table per request; fix = filtered store list method (backend-parity family, do
  with the other store-trait additions + conformance).
- Consent UX follow-ons (additive): `LIGHTTRACK_COLLECTIVE_CONTRIBUTE` master switch + `contributable`
  stamp so the CLI refuses to POST; `DELETE /v1/collective/contribution` (right-to-withdraw, ~20 lines
  on existing `derive_contributor_id` + `delete_collective_entries`); digest scope headline in render;
  `received_at` freshness/retention (collective-api #3 High).
- Redaction audit trail (event-ingestion #2 remainder): stderr-only logging → a queryable provenance
  stamp (e.g. `metadata.redaction_applied`), and surface the *effective* policy (project ∨ env floor)
  in the projects table.

## ✅ DONE (Wave 2 session): money-truth query scans

- cost-forecasting #1 (Critical) + revenue-margin-tracking #2 (High) closed in `7b11ccc` — sargable
  `project_pred()` across all six SQLite money-path queries, pinned by an EXPLAIN QUERY PLAN test.
- margin-simulation-fx #1 (Critical, Firestore) closed in `c2d2fd3` — window pushed into
  `cost_by_dimension`'s query (mirrors `usage_since`'s existing shape; rides the already-required
  `(project_id, ts)` composite index). **Compile-verified; verify live via the emulator.**
- event-ingestion-query #1 (Firestore per-ingest aggregation) **REFRAMED, not fixed**: documented
  design tradeoff in `docs/FIRESTORE.md` (client-side aggregation fine at ≤1k calls/hr; at-scale
  mitigation = rollup counter docs / EventSink). Revisit only when target load grows; follow the doc's
  own mitigation plan, not ad-hoc `runAggregationQuery` bolt-ons.

## ⚠ OPEN: UsageCache read-path wiring (budget-limits #1, High — theme T4)

`/v1/limits/status` (`evaluate_project_limits`) recomputes full rolling-window SUM/COUNT under the
global SQLite mutex on every poll, ignoring the incremental `UsageCache` that only
`insert_event_checked` maintains. Wiring reads into the cache needs a cache-authority decision (plain
`insert_event` bypasses it → staleness). Own focused session; touches `sqlite/{usage_cache,events}.rs`
+ `store/src/lib.rs` read path.

## ✅ DONE (Wave 4 session): score-recording #1 (Critical) + #2 (High)

Closed in `a62b792`. Server-side scoped anti-join: `Store::scored_event_ids` (required, all 3 backends) +
`Store::list_unscored_events` (correct default) + `GET /v1/events?unscored=1` + runner rewrite +
`idx_scores_event` in both schemas. SQLite conformance verifies the new methods; PG/Firestore impls are
compile-verified only (need a live DB).

## ✅ DONE (Wave 4 session): store-trait conformance-gap (High)

Closed in `bcc23dc`. The conformance suite now exercises `list_events_filtered`, `cost_summary_windowed`,
`usage_since_scoped`, `usecase_costs`. SQLite passes; the suite will now correctly FAIL PG/Firestore live.

## ⚠ OPEN: implement the default-bearing query methods on Postgres + Firestore

The conformance suite now encodes the correct contract, and PG/Firestore inherit *wrong* defaults for:
`list_events_filtered` (returns unfiltered), `cost_summary_windowed` (returns all-time),
`usage_since_scoped` (falls back to project-wide), `usecase_costs` (returns empty). They must each override
these. **Requires a live Postgres + Firestore to verify** — cannot be done on the Windows dev box, which is
why it was not attempted this session (writing untested query code across two backends is the exact risk the
audit warns about).

- **Postgres**: mirror the SQLite SQL (`crates/store/src/sqlite/events.rs`) with `$N` placeholders / `sqlx`.
  `list_events_filtered` (keyset paging on `(ts,id)`), `cost_summary_windowed` (windowed GROUP BY),
  `usage_since_scoped` (add the scope WHERE clause). **`usecase_costs` also needs a schema change**: the PG
  `events` table has no `name` column (event-ingestion-query #1), so ingest must persist `name` and the query
  group by it — bigger than the other three.
- **Firestore**: REST structured queries. Windowed/scoped are range+equality filters (composite index needed);
  `usecase_costs` groups client-side after a windowed fetch. Keyset paging on `(ts,id)` via `startAfter`.
- **Verification**: run `cargo test -p lighttrack-store-pg` / `-firestore` with the live-DB env var set; the
  conformance `parity_gap_methods` section is the acceptance test.

## ⚠ OPEN: Firestore transport batch write (cloud-store-backends.perf #1, High)

`commit_update` hard-codes a 1-element writes array, so trait-default batch methods loop one HTTP RTT per
doc (and are non-atomic on the revenue path). Use `:commit`'s 500-write batch. Firestore-only; needs live
verification. Not started.

## Notes for whoever resumes

- Branch `vibeman/perf-feature-2026-07-16` holds Wave 1 (4 commits) + Wave 4 (3 commits: a62b792, bcc23dc,
  plus this doc), off `main`, not pushed.
- The "1000-row cap silently breaks correctness" shape (INDEX theme T7) still appears in relay dead-letter
  lookup and the calibration drift-window — same fix pattern as the score anti-join, worth a sweep.
- Remaining waves: 2 (money-truth Firestore/forecast scans), 3 (privacy & consent), 5–8. See INDEX.md.
