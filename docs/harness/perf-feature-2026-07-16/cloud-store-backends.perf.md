# Performance Optimizer — Cloud Store Backends (Postgres + Firestore)

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Firestore transport has no batch-write path — every write is its own HTTP round trip
- **Severity**: High
- **Category**: unbatched-write
- **File**: `crates/store-firestore/src/rest.rs:106-135` (`commit_update`), also `put_doc`/`patch_fields:46-64`
- **Scenario**: A Polar/Stripe webhook delivers a batch of revenue events; `insert_revenue_events` runs. Prices seeding writes the whole model-price table; dataset creation writes N items. All go through the store one document at a time.
- **Root cause**: The transport only exposes single-document operations. `commit_update` hard-codes `"writes": [write]` — a one-element array — and `put_doc`/`patch_fields` each issue one PATCH. There is no helper that packs multiple `writes` into a single `:commit`. So the `Store` batch methods that fall back to the trait default loop (`insert_revenue_events` at `store/src/lib.rs:574`, `insert_events_checked`, dataset seeding) become N independent HTTP calls. The Firestore `:commit` endpoint already accepts up to 500 writes in one request.
- **Impact**: For a batch of N docs: N × (TCP/TLS-amortized) HTTP round trips instead of ~⌈N/500⌉. At a ~30–80 ms RTT to `firestore.googleapis.com`, a 50-event revenue webhook spends ~1.5–4 s wall-clock serially versus a single commit. Billed writes are unchanged (still N doc writes), but latency and connection churn scale linearly with batch size, and the revenue path is additionally **non-atomic** — a mid-batch failure leaves 1..k committed and k+1..N lost, exactly the hazard the trait doc warns about. A batched `:commit` fixes both cost and atomicity at once.
- **Fix sketch**: Add `commit_batch(writes: &[Value]) -> Result<()>` to `Rest` that posts `{ "writes": [...] }` (chunk at 500) to `{base}:commit`, and override `insert_revenue_events` (and other true-batch methods) in `FirestoreStore` to build all typed-field writes and issue one commit per chunk. A single-chunk commit is also atomic, closing the partial-write gap.
- **Trade-offs**: Do **not** batch `insert_events_checked` — its per-item commit-before-next-admission-read is a deliberate cap-honesty choice (documented at `store/src/lib.rs:300-306`). Scope batching to methods that want all-or-nothing semantics.

## 2. Postgres pool hard-capped at 5 connections, not configurable
- **Severity**: High
- **Category**: no-pool-reuse
- **File**: `crates/store-pg/src/lib.rs:55-62`
- **Scenario**: The API ingests LLM events under concurrent load; each store call runs on a `spawn_blocking` worker and does `rt.block_on(query)`, holding a pooled connection for the query's duration.
- **Root cause**: `PgPoolOptions::new().max_connections(5)` is a fixed literal with no env override and no `min_connections`/`acquire_timeout`. The whole process — every ingest write, every dashboard aggregation, every job-queue `claim_job` — shares at most 5 live Postgres connections. Beyond 5 in-flight queries, callers block in `acquire()` (with sqlx's default 30 s acquire timeout) regardless of how many blocking workers the API is willing to run.
- **Impact**: Process-wide DB concurrency is throttled to 5. A burst of ingest plus one slow `cost_summary`/`cost_by_dimension` aggregation scan can saturate the pool and stall unrelated writes; effective throughput is capped at `5 / mean_query_latency` (e.g. ~250 qps at 20 ms/query) no matter the host size. Managed Postgres (Cloud SQL / RDS / Neon) typically allows dozens–hundreds of connections, so this leaves headroom unused.
- **Fix sketch**: Read `max_connections` from env (e.g. `LIGHTTRACK_PG_MAX_CONNECTIONS`) with a higher default (~10–20, sized to the deployment's blocking-worker count and the DB's `max_connections`); set `min_connections` for warm reuse and an explicit `acquire_timeout` so saturation fails fast instead of hanging 30 s. Measure with pool `size()`/`num_idle()` and connection-acquire wait time.
- **Trade-offs**: Raising the cap must stay under the server's connection ceiling (and any PgBouncer limit); pooler-fronted deployments may prefer transaction pooling over a large app-side pool. None material below the DB ceiling.

## 3. Firestore read path double-allocates every document (clone + full re-decode)
- **Severity**: Medium
- **Category**: codec-cost
- **File**: `crates/store-firestore/src/rest.rs:82-101` (`query_raw`/`query`) + `crates/store-firestore/src/codec.rs:52-114`
- **Scenario**: A list/dashboard call (`list_events`, `list_scores`, `list_prices`, `cost_summary`) runs a `runQuery` returning up to `limit` documents, each with ~10–15 typed fields.
- **Root cause**: `query_raw` clones every matched document out of the parsed response array (`out.push(doc.clone())` at rest.rs:96), producing an owned `Vec<Value>`; then `query` walks each cloned doc again through `decode_doc`, and `decode_value` rebuilds every field into a fresh plain-JSON tree (e.g. `integerValue` string → `parse::<i64>` → `json!(n)`, arrays/maps rebuilt node-by-node). That is two full JSON-tree materializations per document plus the initial `json_ok` `text()`→`from_str` buffering — three passes over the same bytes before the domain mapper even runs.
- **Impact**: On the read hot path, per-document allocation is ~2× what a single decode needs, plus per-field small allocations. Bounded by the query `limit` (so not runaway), but it is pure overhead on the most-hit surfaces (dashboards, event lists) and adds GC/allocator pressure under concurrent reads. No billed-read change — this is CPU/allocation only.
- **Fix sketch**: Have `query` consume the array by value (`into_iter().map(|d| decode_doc(&d))`) instead of cloning into an intermediate `Vec<Value>`; keep `query_raw`'s clone only for the `claim_job` path that genuinely needs the raw doc (`name`/`updateTime`). Optionally decode directly from the borrowed response `Value` array in `json_ok` to drop the intermediate entirely. Verify with an allocation profile (dhat) on a `list_events(limit=1000)` call.
- **Trade-offs**: `claim_job` still needs the raw document, so the raw variant stays; the win is confined to the decoded-`Fields` callers. None material.
