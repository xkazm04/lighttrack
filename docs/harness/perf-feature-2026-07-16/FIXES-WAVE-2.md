# Perf+Feature Fix Wave 2 — Money-truth query scans (themes T2-perf/T4)

> 2 commits. Closed 2 of the 3 wave-2 criticals + 1 High; the third critical was investigated and
> REFRAMED as documented-by-design (see below). Baseline preserved: workspace 425 passed / 0 failed.
> Branch `vibeman/perf-feature-2026-07-16` (off `main`, NOT pushed) — 12 commits total across Waves 1/4/2.

## Mental model

The money-path reads (forecast, margin, budget) scale with **total retained history instead of the
requested window**, because the query predicate defeats the index (SQLite) or the window never reaches
the query at all (Firestore). Every fix here is "make the window/project reach the storage engine."

## Commits

| # | Commit | Finding(s) closed | Sev | Files |
|---|---|---|---|---|
| 1 | `7b11ccc` | cost-forecasting #1 (Critical) + revenue-margin-tracking #2 (High) | Critical + High | `store/src/sqlite/{mod,forecast,revenue}.rs` |
| 2 | `c2d2fd3` | margin-simulation-fx #1 (Critical, Firestore) | Critical | `store-firestore/src/revenue.rs` |

## What was fixed

1. **SQLite: sargable project predicate on all six money-path queries.** The
   `(?1 IS NULL OR project_id = ?1)` form defeats `idx_events_project_ts` even when a project IS given —
   the OR with a non-column condition forces a full scan of `events` across all projects and all time,
   under the single global connection mutex, on every forecast poll and margin view (plus per-row
   `json_extract`). New `project_pred()` helper emits `project_id = ?1` (index seek) or `?1 IS NULL`
   (constant TRUE — the all-projects path is inherently a scan); same `?1` binding, callers unchanged.
   Applied to `daily_cost_by_dimension`, `cost_by_dimension`, `tokens_by_dimension`, the revenue
   recognition `list`, and both per-customer breakdowns. **Pinned by an EXPLAIN QUERY PLAN regression
   test**: the sargable form must ride the index; the old OR form is asserted NOT to.

2. **Firestore: the recognition window now reaches the query.** `cost_by_dimension` filtered only by
   `project_id` and applied `[since, until)` client-side — every margin/simulate/trend/forecast request
   read the project's ENTIRE event history in billed doc reads (~$3/request at 5M events). Added the
   `ts` range filters, mirroring the exact `project_id EQUAL + ts range` shape `events::usage_since`
   already runs. The fixed-width RFC3339 timestamps make the lexicographic range filter correct (the
   documented reason for that encoding), and the query rides the `(project_id, ts)` composite index
   `docs/FIRESTORE.md` already requires for deployment — **no new index/deploy dependency**. Client-side
   window re-check kept as belt-and-suspenders. Compile-verified; live verification via the emulator
   (per FIRESTORE.md's own plan) is tracked follow-up.

## Verify-before-fix: the third critical reframed

**event-ingestion-query #1 (Firestore `usage_since` reads the whole rolling window per ingest)** turned
out to be a **documented design tradeoff**, not an oversight: `docs/FIRESTORE.md` explicitly chose
client-side aggregation ("O(matched-docs) reads — fine at the target load (≤1k calls/hr)") and names
the at-scale mitigation (rollup counter docs, or the analytical EventSink). The scan's severity assumed
no design intent. The finding remains real *at scale*, but the honest disposition is
**documented-by-design with a planned mitigation path** — implementing `runAggregationQuery` REST
support now would be new, untestable-here infrastructure contradicting a recorded decision. Recorded in
followups as an at-scale item, pointing at the doc's own mitigation plan.

## Verification

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean |
| workspace tests | 425 passed / 0 failed (added the query-plan regression test) |
| Query plan | `EXPLAIN QUERY PLAN` asserts index seek for the sargable form (test-pinned) |

## Patterns established (catalogue, continued)

7. **Sargability check on OR-optional filters** — `(?N IS NULL OR col = ?N)` is a full-scan even when
   the param is provided. Emit the predicate conditionally (`col = ?N` / `?N IS NULL`) with an unchanged
   binding; pin with an EXPLAIN QUERY PLAN test asserting the index is used.
8. **Verify a finding against design docs before fixing** — a scan finding that contradicts a recorded
   decision (docs/FIRESTORE.md's client-side-aggregation tradeoff) is a *reframe*, not a fix.

## What remains (wave-2-adjacent)

- **UsageCache read-path wiring (budget-limits #1, High)** — `/limits/status` recomputes rolling-window
  SUM/COUNT instead of consulting the incremental cache (which only `insert_event_checked` maintains).
  An architectural change (cache authority/staleness across plain `insert_event`) — own focused session.
- Firestore live verification (emulator) for `c2d2fd3` + the Wave-4 items; forecast memoization /
  per-key materialization Highs. See `followups-2026-07-16.md`.
