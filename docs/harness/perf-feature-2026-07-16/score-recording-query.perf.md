# Performance Optimizer — Score Recording & Query

> Total: 3
> Critical: 1 | High: 1 | Medium: 1 | Low: 0

## 1. Client-side anti-join with a hard 1000-score cap re-judges events and burns judge credits at scale
- **Severity**: Critical
- **Category**: anti-join
- **File**: `crates/runner/src/score.rs:59-84`
- **Scenario**: A project with >1000 stored scores runs `score` (especially the `--interval` online loop, or a backlog pass with a large `--limit`). Judge model is a paid Claude/Agent-SDK call per event (`cost_usd` recorded per score).
- **Root cause**: `score_once` finds "unscored" events by fetching events (`/v1/events?limit=N`, ordered `ts DESC`) and *separately* fetching scores (`/v1/scores?limit=1000` — hardcoded, and the API additionally clamps to `.min(1000)` in `api/src/scores.rs:45`). It builds a `HashSet` of `event_id`s from those ≤1000 rows and skips events found in it. This is a two-table anti-join done client-side over HTTP, and the "already-scored" side is truncated to the 1000 most-recent scores. Once a project accrues more than 1000 scores, an event whose score has aged out of that window is no longer in `scored_ids`, so if it is still returned by the events query it gets **re-judged**. The idempotency guarantee — the command's core promise — silently degrades exactly when the table is large. Per cycle the loop also transfers up to 1000 full `Score` rows (id, rubric, `reasoning` free-text, etc.) purely to extract `event_id`, and re-does it every `--interval` tick even when zero new events arrived.
- **Impact**: Duplicate paid judge calls (unbounded $ as scores grow past 1k); on Firestore the dedup fetch alone is up to 1000 billed doc reads *every interval*, indefinitely, for a project that may have nothing new to score. Payload transfer is O(1000 × row size) per cycle vs O(matched-ids) needed.
- **Fix sketch**: Replace the client anti-join with a server-side "unscored events" query: `SELECT e.* FROM events e LEFT JOIN scores s ON s.event_id = e.id WHERE s.id IS NULL [AND e.project_id=?] ORDER BY e.ts DESC LIMIT N` (add an `/v1/events?unscored=1` param or a dedicated endpoint), backed by the `scores.event_id` index from finding #2. This removes the 1000-cap correctness hole, the per-cycle bulk transfer, and the repeated Firestore reads.
- **Trade-offs**: Firestore has no server-side anti-join — for that backend keep a client dedup but page it fully (or store a `scored` flag / query `scores` by the specific event ids in the fetched page instead of a blind top-1000). SQL/PG get it for free.

## 2. `list_by_trace` join has no index on `scores.event_id` — full scan of the scores table per trace view
- **Severity**: High
- **Category**: missing-index
- **File**: `crates/store/src/sqlite/scores.rs:38-49` (schema: `schema/sqlite/001_init.sql:70-83`, `schema/postgres/001_init.sql:63-76`)
- **Scenario**: Rendering a trace's scores via `SELECT ... FROM scores s JOIN events e ON s.event_id = e.id WHERE e.trace_id = ?1`. Called on trace-detail rendering; scores table grows unbounded with judge activity.
- **Root cause**: Schema defines only `idx_scores_project ON scores(project_id, created_at)`. There is no index on `scores.event_id`. The planner resolves the trace's events via `idx_events_trace`, but joining to `scores` on `event_id` then has no index to probe, forcing a full scan of `scores` (once, or per event row) for every trace-detail query.
- **Impact**: Trace-detail latency grows linearly with total score count — O(rows in scores) per view instead of O(scores-for-this-trace). At 10⁵–10⁶ scores this is tens–hundreds of ms of scan per render on the global `Mutex<Connection>`, also stalling concurrent readers.
- **Fix sketch**: `CREATE INDEX IF NOT EXISTS idx_scores_event ON scores(event_id);` in both `schema/sqlite/001_init.sql` and `schema/postgres/001_init.sql`. Turns the join into an index probe and also directly enables finding #1's server-side anti-join.
- **Trade-offs**: One extra index to maintain on insert (negligible; scores are low-write). None material.

## 3. Project-agnostic `list(None, limit)` full-scans and sorts the entire scores table
- **Severity**: Medium
- **Category**: table-scan
- **File**: `crates/store/src/sqlite/scores.rs:66-72`, `crates/store-pg/src/scores.rs:48-53`
- **Scenario**: Any scores listing without a project filter — an admin/global view, or the runner's own dedup fetch when invoked without `--project` (`spath = "/v1/scores?limit=1000"`, `runner/src/score.rs:60`).
- **Root cause**: The no-project branch runs `SELECT ... FROM scores ORDER BY created_at DESC LIMIT ?`. The only index, `idx_scores_project(project_id, created_at)`, leads with `project_id`, so it cannot satisfy a `project`-agnostic `ORDER BY created_at`. The engine must scan every row and top-N sort to return the newest `limit`.
- **Impact**: Every global score list is O(all scores) scan + sort, even to return 50 rows — repeated each `--interval` cycle when the runner is run project-less, and holding the SQLite global mutex the whole time.
- **Fix sketch**: Add `CREATE INDEX idx_scores_created ON scores(created_at);` so the newest-N read is an index range scan, or require/scope a project on the global path. Cheap given low write volume.
- **Trade-offs**: A second small index. Alternatively skip if global listing is never a real access pattern — but the runner's own default (no `--project`) exercises it, so it is real.
