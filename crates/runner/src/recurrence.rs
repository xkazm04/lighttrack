//! Opt-in recurrence for benchmarks: a sweep, run from inside `lt-runner serve`, discovers benchmarks
//! whose `target.schedule_interval_secs` is set and enqueues a `bench_run` when one is due — turning a
//! benchmark into continuous quality monitoring without any cron in the benchmark path itself.
//!
//! The sweep is **idempotent**: a benchmark is due only when it has no queued/running `bench_run`
//! *and* its most recent run finished longer ago than the interval, so repeated sweeps never pile up
//! jobs (mirrors the dataset scheduler's idempotency, `schedule.rs`). Discovery reuses existing read
//! endpoints — list projects → list benchmarks → list runs/jobs — with the runner's admin key, and
//! enqueue reuses the existing `POST /v1/benchmarks/:id/enqueue` path (no new job type).

use std::collections::HashSet;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{Benchmark, BenchmarkRun, Job, Project};

use crate::cli::Cli;
use crate::http::{get, post};
use crate::util::short;

/// Reserved key under a benchmark's free-form `target` carrying its recurrence interval in seconds
/// (kept in sync with the API's `benchmarks::RECURRENCE_KEY`).
const RECURRENCE_KEY: &str = "schedule_interval_secs";

/// Read a benchmark's opt-in recurrence interval (seconds) from its `target`, or `None` when unset —
/// including a matrix/array `target` that can't carry it. Zero is treated as disabled.
pub(crate) fn recurrence_secs(bench: &Benchmark) -> Option<u64> {
    bench.target.get(RECURRENCE_KEY).and_then(Value::as_u64).filter(|s| *s > 0)
}

/// Whether a recurring benchmark is due to run now. Pure, so it is unit-testable in isolation:
/// - `interval_secs == 0` → never due (disabled),
/// - an in-flight (queued/running) `bench_run` → not due (idempotent: don't stack jobs),
/// - never run → due,
/// - otherwise due once `now - last_run >= interval`.
pub(crate) fn is_due(
    interval_secs: u64,
    last_run: Option<DateTime<Utc>>,
    has_inflight_job: bool,
    now: DateTime<Utc>,
) -> bool {
    if interval_secs == 0 || has_inflight_job {
        return false;
    }
    match last_run {
        None => true,
        Some(t) => now.signed_duration_since(t).num_seconds() >= interval_secs as i64,
    }
}

/// One recurrence sweep: enqueue a `bench_run` for every due recurring benchmark across all enabled
/// projects. Returns the number enqueued. A failed HTTP call aborts the sweep with an error; the
/// caller (`serve`) logs it and keeps polling, so a transient API blip never kills the worker.
pub(crate) fn check_and_enqueue(cli: &Cli, http: &reqwest::blocking::Client) -> Result<usize> {
    let now = Utc::now();
    let inflight = inflight_bench_ids(cli, http)?;
    let projects: Vec<Project> = get(cli, http, "/v1/projects")?;
    let mut enqueued = 0;
    for p in projects.iter().filter(|p| p.enabled) {
        let benches: Vec<Benchmark> = get(cli, http, &format!("/v1/projects/{}/benchmarks", p.id))?;
        for b in &benches {
            let Some(interval) = recurrence_secs(b) else {
                continue;
            };
            let has_job = inflight.contains(&b.id);
            // Skip the run-history fetch when a job is already in flight (it can't be due anyway).
            let last = if has_job { None } else { latest_run_time(cli, http, &b.id)? };
            if is_due(interval, last, has_job, now) {
                post(cli, http, &format!("/v1/benchmarks/{}/enqueue", b.id), &json!({ "samples": 1 }))?;
                println!("recurrence: enqueued bench_run for {} ({})", short(&b.id), b.name);
                enqueued += 1;
            }
        }
    }
    Ok(enqueued)
}

/// benchmark_ids that already have a queued or running `bench_run` job, so a sweep never stacks a
/// second one on top of work that hasn't finished.
fn inflight_bench_ids(cli: &Cli, http: &reqwest::blocking::Client) -> Result<HashSet<String>> {
    let mut set = HashSet::new();
    for status in ["queued", "running"] {
        let jobs: Vec<Job> = get(cli, http, &format!("/v1/jobs?status={status}&limit=1000"))?;
        for j in jobs {
            if j.job_type == "bench_run" {
                if let Some(id) = j.payload.get("benchmark_id").and_then(Value::as_str) {
                    set.insert(id.to_string());
                }
            }
        }
    }
    Ok(set)
}

/// Timestamp of a benchmark's most recent run: its `finished_at`, or `started_at` as a fallback for a
/// still-running/legacy run. `None` when it has never run (→ due immediately). Runs come back
/// newest-first (the store orders by `started_at DESC`).
fn latest_run_time(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    id: &str,
) -> Result<Option<DateTime<Utc>>> {
    let runs: Vec<BenchmarkRun> = get(cli, http, &format!("/v1/benchmarks/{id}/runs"))?;
    Ok(runs.first().map(|r| r.finished_at.unwrap_or(r.started_at)))
}

#[cfg(test)]
mod tests {
    use super::{is_due, recurrence_secs};
    use chrono::{Duration, Utc};
    use lighttrack_core::Benchmark;
    use serde_json::{json, Value};

    fn bench(target: Value) -> Benchmark {
        serde_json::from_value(json!({ "name": "n", "rubric": "r", "target": target })).unwrap()
    }

    #[test]
    fn reads_interval_from_target() {
        assert_eq!(recurrence_secs(&bench(json!({ "schedule_interval_secs": 3600 }))), Some(3600));
        // Zero / missing / null / a matrix array all read as "no recurrence".
        assert_eq!(recurrence_secs(&bench(json!({ "schedule_interval_secs": 0 }))), None);
        assert_eq!(recurrence_secs(&bench(json!({ "endpoint": "x" }))), None);
        assert_eq!(recurrence_secs(&bench(json!(null))), None);
        assert_eq!(recurrence_secs(&bench(json!([{ "provider": "o", "model": "m" }]))), None);
    }

    #[test]
    fn disabled_and_inflight_are_never_due() {
        let now = Utc::now();
        assert!(!is_due(0, None, false, now)); // interval 0 = disabled
        assert!(!is_due(3600, None, true, now)); // in-flight job → don't stack
        assert!(!is_due(3600, Some(now - Duration::seconds(10_000)), true, now)); // stale but in-flight
    }

    #[test]
    fn due_when_never_run_or_older_than_interval() {
        let now = Utc::now();
        assert!(is_due(3600, None, false, now)); // never run
        assert!(is_due(3600, Some(now - Duration::seconds(3601)), false, now)); // just past interval
        assert!(is_due(3600, Some(now - Duration::days(2)), false, now)); // long past
    }

    #[test]
    fn not_due_within_interval() {
        let now = Utc::now();
        assert!(!is_due(3600, Some(now - Duration::seconds(100)), false, now));
        // Exactly at the boundary counts as due (>=).
        assert!(is_due(3600, Some(now - Duration::seconds(3600)), false, now));
    }
}
