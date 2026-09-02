# M7 — Typed job queue with stored schedules; relay adopts the fenced, renewable lease

Size XL · gate contract · wave B · contexts: runner-worker, benchmark-management, relay-queue,
device-agent, api-server, store-sqlite-eval, store-postgres

## Problem
Two halves, one spine.
**Jobs**: `crates/api/src/jobs.rs` ~64 hard-codes `job_type: "bench_run"`; `crates/runner/src/serve.rs`
~325-397 handles only that kind. The runner ships five separately scheduled loops with private
`--interval/--once` cadence (`crates/runner/src/cli.rs`: score, score-traces, schedule, calibrate
--watch, recur). Benchmark recurrence is smuggled into `target.schedule_interval_secs`
(`core/score.rs` ~226 `RECURRENCE_KEY`; `runner/recurrence.rs` ~25-33 admits an array `target`
cannot carry it, so **compare benchmarks — the headline mode — cannot recur**). The lease/cancel/
fencing machinery protects only benchmark runs; the API already hosts detached sweeps
(`main.rs` ~344-351: forecast, storage), so the scheduler belongs there.
**Relay**: `RelayTask` is the design `Job` outgrew (`core/job.rs` ~44-58 explains the fence):
attempts consumed on lease (`core/relay.rs` ~80-82), no renew/cancel/progress, settle checks only
`status == "leased"` (`sqlite/relay.rs` ~137; same on PG) so a stale device's late report lands on
a task re-leased elsewhere; `lease_secs` clamp 60..21600 is both detection latency and max run
(`api/relay.rs` ~203-204); `sweep_relay_dead` runs only inside `lease_tasks` (~209-214), so with no
device polling nothing dead-letters or alerts.

## Design
### Jobs
1. `crates/core/src/job.rs`: `pub enum JobKind { BenchRun, ScoreEvents, ScoreTraces, DatasetSample, Calibrate }`
   with `#[serde(rename_all="snake_case")]` so the wire string `"bench_run"` is unchanged; typed
   payload structs per kind in `crates/core/src/job_kinds.rs`. `Job.job_type` stays a `String` on
   the row; `Job::kind() -> Option<JobKind>`.
2. `crates/core/src/schedule.rs`: `Schedule { id, project_id, kind, payload, interval_secs, next_due, last_job_id, enabled, created_at }`.
3. Store: `create_schedule`, `list_schedules(project)`, `update_schedule`, `due_schedules(now)`,
   `claim_job(stale_secs, kinds: &[&str])` (filter by kind; PG `WHERE type = ANY($kinds)` inside
   the existing `FOR UPDATE SKIP LOCKED`). Implement SQLite + Postgres; Firestore `Unsupported`
   for schedules (declare in the M1 manifest as a `Schedules` surface). `schedules` table in
   `schema/sqlite/001_init.sql` + `schema/postgres/001_init.sql` (+ `ADDED_COLUMNS`/ALTER lists if
   any column is added to `jobs`). One-time migration: for every benchmark carrying
   `RECURRENCE_KEY`, write a `Schedule` row (SQLite/PG); keep reading the key for one release.
4. API: `crates/api/src/schedule_sweep.rs` (pattern of `forecast_sweep`; on by default — it is
   upkeep of the operator's own declared schedules; env `LIGHTTRACK_SCHEDULE_SWEEP_SECS` to tune/
   disable): for each due schedule enqueue a job unless one for that schedule is queued/running
   (existing idempotency rule), set `next_due`. Routes: `POST /v1/jobs` (admin, kind-validated),
   `POST/GET /v1/projects/:id/schedules`, `PUT/DELETE /v1/schedules/:id`, `GET /v1/schedules/:id/runs`.
   `enqueue_benchmark` becomes `enqueue(JobKind::BenchRun{..})`. `POST /v1/jobs/claim` body gains
   `kinds: [..]`, `providers: [..]` (worker capability declaration; empty = all, for old runners).
5. Runner: `process_job` dispatches on `JobKind` → existing `score::score_recent`,
   `score_traces::run` (one cycle), `dataset::build_from_events`, `calibrate_watch` (one cycle);
   each inherits `RunControl` (cancel/progress/lease) for free. `serve --kinds a,b --providers x`
   declares capabilities (default: all kinds; providers derived from which API keys are present in
   env). The five subcommands stay as thin wrappers: enqueue-then-serve-once when `--via-queue`,
   else the current direct path (do not break existing invocations this wave). Delete
   `recurrence.rs` once the migration writes schedules.
6. MCP: `list_schedules` (read), `create_schedule`, `enqueue_job` (write-gated). CLI: `lt schedules`,
   `lt jobs`. Render: schedules table.
### Relay lease
7. `crates/core/src/lease.rs`: `LeaseFence(DateTime<Utc>)` + `LeaseHeld` verdict; `RelayTask +=
   lease_fence: Option<DateTime>, failures: u32, progress: Option<String>`; `RelayStatus +=
   Cancelling, Cancelled` (keep `ALL` exhaustive). `Job` re-expresses its `claimed_at` fence
   through the same type (no behaviour change).
8. Store: `renew_relay_lease(id, fence) -> Option<DateTime>`, `cancel_relay_task(id)`,
   `update_relay_progress(id, fence, text)`, and `settle_relay_task(id, fence, outcome)` becomes a
   conditioned write returning `NotHeld` on fence mismatch. Lease stops incrementing `attempts`;
   `failures` increments on `Failed`; dead-letter on `failures >= max_attempts` or
   `stale_reclaims >= N` (mirror `job.rs` ~32-36). SQLite + PG; Firestore stays `Unsupported` for
   relay. Extend the conformance relay section: "late settle after reclaim is NotHeld",
   renew extends deadline, cancel from queued/leased, progress visible in `get_relay_task`.
9. API: `POST /v1/relay/tasks/:id/renew` (device), `/progress` (device), `/cancel` (project key
   own / admin; 409 on terminal); settle body carries `fence`; 409 `not held` on mismatch (same
   shape as `jobs.rs` ~262-263). Lease response returns `renew_secs`. Move `sweep_relay_dead` +
   its alert onto the schedule sweep above (keep the pre-lease call).
10. Agent: `run.rs` spawns a renewal thread (TTL/3, copy `runner/serve.rs` ~122-132) around
    `exec::execute`; treats `409 not held` as "stop, do not deliver via connector".
11. Docs: `docs/SCHEDULING.md` rewritten around stored schedules; `docs/RELAY.md` lease section;
    `docs/BENCHMARK_FRAMEWORK.md` recurrence section.

## Out of scope
Device enrolment/capabilities (M18). Relay cost pricing (M5). Alert persistence (M3).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -runner,
-agent, -mcp, -cli, -render; SQLite conformance (including the new relay cases); existing
`tests_relay.rs` updated, not weakened.

## Evaluation
Before: `process_job` handles 1 kind; 5 daemon loops; compare benchmarks cannot recur; relay settle
unfenced; relay detection latency = lease_secs (≤6 h). After: 5 kinds via the queue; `GET
/v1/schedules` lists every recurring workload incl. compare; conformance proves late settle is
`NotHeld`; staleness window ≈ 3× renew interval.
