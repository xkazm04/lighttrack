# Performance Optimizer — Background Job Queue

> Total: 3
> Critical: 0 | High: 1 | Medium: 2 | Low: 0

Scope note: the SQLite `jobs` table IS indexed (`idx_jobs_status ON jobs(status, created_at)`,
`schema/sqlite/001_init.sql:124`) and the claim is a single atomic `UPDATE … RETURNING` with an
index-backed subquery (sqlite `jobs.rs:40-54`; PG uses `FOR UPDATE SKIP LOCKED`, `store-pg/jobs.rs:42-60`;
Firestore uses an `updateTime` CAS). So claim is **not** a table scan and **not** a non-atomic
SELECT-then-UPDATE race — those candidates were checked and deliberately dropped. The real costs are the
poll cadence, unbounded table growth on the unfiltered list, and Firestore per-poll query amplification.

## 1. Fixed-interval busy-poll claim loop — no backoff, no event wake
- **Severity**: High
- **Category**: busy-poll
- **File**: `crates/runner/src/serve.rs:32-72` (idle sleep at `:67-71`); backs onto `crates/api/src/jobs.rs:125-135` → `crates/store/src/sqlite/jobs.rs:40-54` under the global `Mutex<Connection>` (`crates/store/src/sqlite/mod.rs:49,83`)
- **Scenario**: `lt-runner serve` runs continuously with `--interval 5` (default). Queue is idle most of the day between scheduled benchmark/scoring runs. On the Firestore backend the same loop drives billed reads.
- **Root cause**: When `claim` returns `None` the loop unconditionally `sleep(interval)` and re-polls — a fixed 5s cadence with no exponential backoff and no cross-process notification on enqueue. Every poll is a full HTTP round-trip → `spawn_db` → a claim `UPDATE … RETURNING` that must acquire the *single* global SQLite connection mutex shared with the ingestion (money) path. The claim query itself is cheap (index-backed), but the wakeups are unconditional and never widen when the queue stays empty.
- **Impact**: ~17,280 idle polls/day/worker. On SQLite each serializes on the one connection mutex against ingest — low per-poll (one indexed query) but strictly wasted lock acquisitions that scale ×N workers. On Firestore each idle poll costs **2 billed queries** (see Finding 3) → ~34.5k reads/day/worker of pure idle polling. Plus up to `interval` (5s) added latency before a freshly-enqueued benchmark starts. Busy-throughput is fine (loop re-claims immediately without sleeping after a successful job).
- **Fix sketch**: Exponential idle backoff (e.g. 5s → 30s → 60s cap, reset to 5s on any claim); optionally a long-poll/blocking claim endpoint so an enqueue wakes the worker instead of the next tick. Both cut idle wakeups ~10× with no throughput loss.
- **Trade-offs**: Backoff adds start latency after a long idle stretch (bounded by the cap); a long-poll endpoint adds API complexity. Single-worker SQLite mutex contention alone is minor — the teeth are Firestore read billing, ×N-worker scaling, and start latency.

## 2. No retention + unfiltered `list_jobs` full-scans an unbounded table
- **Severity**: Medium
- **Category**: unbounded-growth
- **File**: `crates/store/src/sqlite/jobs.rs:90-95` (None branch); mirror `crates/store-pg/src/jobs.rs:108-112`; consumed by `crates/api/src/jobs.rs:88-99` and `crates/render/src/jobs.rs:7`
- **Scenario**: Weeks of scheduled scoring + benchmark recurrence (`recur_interval` default 60s sweeps) accumulate `done`/`failed` rows. An admin opens the jobs view without a status filter.
- **Root cause**: Terminal jobs are never deleted or TTL'd — the table grows monotonically. The unfiltered list runs `SELECT … FROM jobs ORDER BY created_at DESC LIMIT n` with no `status` predicate, so `idx_jobs_status(status, created_at)` cannot serve the ordering → SQLite full-scans every row ever written and sorts to return the top n. (The status-filtered branch at `:83` is fine — the index covers it.)
- **Impact**: The unfiltered list cost grows O(total jobs ever run) despite the small `LIMIT`, and runs under the global mutex, briefly stalling ingest. DB file and page cache also bloat unboundedly.
- **Fix sketch**: A retention sweep deleting `status IN ('done','failed') AND updated_at < cutoff`; and/or add `CREATE INDEX idx_jobs_created ON jobs(created_at)` so the unfiltered list is an index-backed backward scan of only `LIMIT` rows.
- **Trade-offs**: Retention loses old job history (keep a window, or archive first); the extra index adds a small write cost on the low-volume jobs table — negligible here.

## 3. Firestore claim issues 2 sequential probes per poll and re-queries on every retry round
- **Severity**: Medium
- **Category**: hot-path
- **File**: `crates/store-firestore/src/jobs.rs:56-108`
- **Scenario**: Firestore-backed deployment, one or more workers polling every 5s; occasional contention when a sweep enqueues several jobs at once.
- **Root cause**: `claim_job` loops up to 5 rounds; each round calls `oldest_queued` (one query) and, only if empty, `oldest_stale` (a second query), then a CAS `commit_update`. In the common **idle** case both probes run → 2 billed queries returning nothing per poll. Under **contention** a lost `updateTime` precondition re-enters the loop and re-runs the full candidate query from scratch, so W workers colliding on the same head job cost up to ~5 query+commit rounds each — O(W) read amplification to hand out one job.
- **Impact**: Multiplies Finding 1's poll cost by 2 on the idle path and by up to ~5× on the contended path (Firestore bills a minimum of one read per query, empty or not). Dominant recurring cost on the Firestore backend.
- **Fix sketch**: Fetch a small candidate *page* (e.g. LIMIT 5 queued, ordered) in one query and attempt CAS down the page in-memory before re-querying; skip the `oldest_stale` probe unless `oldest_queued` returned empty AND a stale-window check is actually due (it already short-circuits, but the two could be a single query with an `IN`/`OR`-shaped filter where the composite index allows). Combined with Finding 1's backoff, idle Firestore reads drop by ~20×.
- **Trade-offs**: Paged candidates add a little client logic and can waste a read when the page is stale; acceptable versus the per-poll query cost. Requires the matching Firestore composite index to exist.
