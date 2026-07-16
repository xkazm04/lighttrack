# Performance Optimizer — Scoring Rubrics

> Total: 2
> Critical: 0 | High: 1 | Medium: 1 | Low: 0

## 1. Per-case score POST serializes an otherwise-parallel run
- **Severity**: High
- **Category**: hot-path
- **File**: `crates/runner/src/rubric.rs:75-138` (POST at `:137`)
- **Scenario**: A benchmark run of N cases. Judging is fanned out concurrently via `parallel_map` (line 65), but results are then folded **in case order** and each case does a blocking `post(cli, http, "/v1/scores", &score)?` (line 137) — one synchronous HTTP round-trip per case, strictly sequential. At 200 cases and a ~20 ms server round-trip that is ~4 s of pure serial network latency tacked onto every run; at 1000 cases, ~20 s — all after the expensive judging already finished.
- **Root cause**: The score write is done inside the single-threaded fold loop, so N POSTs happen one-after-another. The parallelism that was carefully arranged for judging (the costly part) is discarded for the cheap-but-serial write phase. Ordering is only needed for the printed log / dim aggregation, not for the network writes.
- **Impact**: Adds O(N) sequential round-trips (~N × RTT) to wall-clock time, independent of judge cost. Linear in case count; dominates for large benchmarks with a fast judge or cached outputs.
- **Fix sketch**: Collect the per-case `score` JSON into a `Vec` during the fold, then either (a) POST a single batched `/v1/scores` array after the loop, or (b) issue the writes concurrently (bounded pool, same `jobs`) once ordering-dependent aggregation is done. Batching (a) is one round-trip total and preserves order server-side.
- **Trade-offs**: Batch endpoint may need to exist server-side; on partial failure the batch semantics (all-or-nothing vs per-row) must be defined. Scores become visible at end-of-run rather than trickling in.

## 2. Rubric reloaded from the store on every run with no cache
- **Severity**: Medium
- **Category**: per-run-reload
- **File**: `crates/runner/src/rubric.rs:40`; store reads `crates/store-firestore/src/rubrics.rs:22-24`, `crates/store/src/sqlite/rubrics.rs:22-27`, `crates/store-pg/src/rubrics.rs:31-38`
- **Scenario**: `run_rubric_benchmark` does `get(cli, http, "/v1/rubrics/{rubric_id}")` at the top of every run. That resolves to `get_rubric` → a store read. On the **Firestore** backend (`get_doc`) this is a **billed document read per run**; a rubric changes rarely (it is versioned config) but is refetched for each of, e.g., a CI matrix of benchmarks run on every commit.
- **Root cause**: The rubric is slow-changing config read on the hot path of run setup, but there is no caching layer between the runner and the store — every invocation pays a fresh read (and, on Firestore, a fresh billed doc read + JSON-string deserialization of `dimensions`).
- **Impact**: One extra round-trip + one billed Firestore read per run. Bounded (one per run, not per case), so cost is modest, but it is pure waste that scales with run frequency across a benchmark fleet.
- **Fix sketch**: Cache resolved rubrics keyed by `rubric_id` with a short TTL (or an ETag/`created_at` conditional fetch) so repeated runs against an unchanged rubric skip the store hit. Even a process-lifetime memo helps a multi-benchmark CLI session.
- **Trade-offs**: Stale rubric if edited mid-session — bound with a small TTL or explicit invalidation on the create/update path. Negligible memory (rubrics are tiny).

## Deliberately not filed
- `dim_mean(...)` is recomputed ~6× per dimension across the reporting/scorecard/healing blocks, but it is a `HashMap` get + division over a handful of dimensions — nanoseconds; caching it would be a micro-optimization, not a bottleneck.
- `list_rubrics` (sqlite/pg/firestore) returns all rows for a project with no `LIMIT`, but rubrics-per-project is small config-scale data; not a real unbounded-result risk here.
- `format!("SELECT {COLS} …")` builds SQL per call — `COLS` is a constant, so this is a trivial allocation, not a hot path; no injection or perf concern.
