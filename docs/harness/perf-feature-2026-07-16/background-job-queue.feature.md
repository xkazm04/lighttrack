# Feature Scout — Background Job Queue

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. No way to cancel a queued or running job
- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/api/src/jobs.rs:82-152`, `crates/core/src/job.rs:16-18`, `crates/api/src/main.rs:286-290`
- **Scenario**: An operator enqueues a `bench_run` against a 300-case dataset, then realizes the wrong judge model / an unbounded prompt is selected. They watch it burn judge calls in `lt jobs` and have no button to stop it — money keeps flowing until it finishes or fails all attempts.
- **Root cause**: The status vocabulary is only `queued | running | done | failed` (job.rs:16-18) and the router exposes only enqueue / list / claim / progress / finish (main.rs:286-290). There is no `cancel` endpoint and no `canceled` state. The store trait (`store/src/lib.rs:511-518`) likewise has no `cancel_job`. Nothing lets an operator remove a queued job or signal a running one to stop — a queued job WILL eventually be claimed and run in full.
- **Impact**: Direct money loss and operator helplessness on exactly the long/expensive workload this queue exists to protect. Cancel is the single most-requested control for any job runner.
- **Fix sketch**: (1) Add a `canceled` status. (2) `POST /v1/jobs/:id/cancel` → store method that flips `queued`→`canceled` unconditionally, and `running`→ sets a `cancel_requested` flag (or `canceled` that `claim` treats as terminal). (3) In `serve.rs::process_job`, between benchmark cases, poll job status / a cancel flag and abort with a `canceled` finish. Queued-job cancel alone (trivial) already delivers most of the value; cooperative running-cancel is the follow-up.
- **Trade-offs**: Running-job cancel needs a cooperative check point inside `run_benchmark`'s case loop; without it, cancel only stops jobs not yet claimed (still worth shipping).

## 2. Progress updates don't refresh the lease → long benchmarks get double-claimed and double-run
- **Severity**: High
- **Category**: resilience
- **File**: `crates/runner/src/serve.rs:115-146`, `crates/store/src/sqlite/jobs.rs:40-62`, `crates/core/src/job.rs:29-31`
- **Scenario**: A `bench_run` with many judge calls legitimately runs longer than `stale_secs` (default **600s / 10 min**, jobs.rs API default at `default_stale_secs`). In a multi-worker deployment (the pg store advertises `FOR UPDATE SKIP LOCKED` "so parallel workers don't grab the same job", pg/jobs.rs:45; firestore has the same multi-worker claim), a second worker's `claim` sees `status='running' AND claimed_at < stale_before` and re-claims the still-alive job — running the entire expensive benchmark a second time.
- **Root cause**: `claimed_at` is documented as the lease "for stale-claim recovery / heartbeat" (job.rs:29), but nothing ever refreshes it after the initial claim. `update_progress` sets `progress` + `updated_at` only, never `claimed_at` (sqlite/jobs.rs:56-62; same in pg/firestore). Worse, the worker posts progress **exactly once** at job start (serve.rs:135-140) and then blocks in `run_benchmark` to completion — so even a lease-refreshing progress call would never fire mid-run. The heartbeat capability is effectively dead: the lease is a fixed 10-min window regardless of real liveness.
- **Impact**: Duplicate concurrent execution of the most expensive jobs = double the judge spend and double-recorded runs, silently. This is the exact failure the lease was meant to prevent.
- **Fix sketch**: (1) Make `update_job_progress` also set `claimed_at = now` (lease renewal), and (2) have `serve.rs::process_job` heartbeat periodically during a long run — e.g. pass a progress callback into `run_benchmark` that posts per-case ("case 42/300"), which now doubles as a lease renewal. Alternatively add a dedicated `POST /v1/jobs/:id/heartbeat`.
- **Trade-offs**: Requires threading a callback into the bench case loop (neighbour file, but the callback is defined here). `stale_secs` must stay comfortably above the heartbeat interval.

## 3. Progress column is plumbed end-to-end but written only once — no live run visibility
- **Severity**: Medium
- **Category**: half-implemented
- **File**: `crates/runner/src/serve.rs:135-141`, `crates/render/src/jobs.rs:20-35,51-53`
- **Scenario**: An operator runs `lt jobs` while a big benchmark executes. The Progress column shows `running benchmark <id>` for the entire 10-minute run — they can't tell if it's on case 3 or case 300, or whether it's wedged.
- **Root cause**: The full progress pipeline exists — DB column, `update_job_progress` store method, `POST /v1/jobs/:id/progress` endpoint, and a rendered "Progress" column in both the list table (render/jobs.rs:20-35) and detail view (render/jobs.rs:51-53). But `serve.rs` posts progress exactly once, before `run_benchmark` starts (serve.rs:135-140); `run_benchmark` prints case-by-case to stdout (bench.rs `run_simple`) but never posts a progress update back. A whole capability is built and then fed a single static string.
- **Impact**: Operators lose the one signal that distinguishes "healthy long run" from "stuck" — the core reason to have a progress field at all. Cheap to light up given the plumbing is already there.
- **Fix sketch**: Thread a `|msg| post(.../progress...)` callback from `process_job` into `run_benchmark`'s case loop and emit `"case {i}/{n}"` (throttled). Pairs perfectly with finding #2's heartbeat — one call both renews the lease and updates visibility.
- **Trade-offs**: One extra POST per case (or per N cases) — throttle to avoid chattiness; none material.
