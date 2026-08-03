//! `serve`: the job-queue worker loop — claim a job, run it (watching for cancellation and
//! publishing live progress), finish it, and retry only what actually failed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::{Job, JOB_ERROR_PREFIX_FAILURE};
use lighttrack_engine::EngineConfig;

use crate::bench::run_benchmark;
use crate::cli::Cli;
use crate::http::post;
use crate::recurrence;
use crate::runctl::{RunControl, CANCEL_POLL_INTERVAL};
use crate::util::short;

#[allow(clippy::too_many_arguments)]
pub(crate) fn serve(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    once: bool,
    interval: u64,
    stale_secs: i64,
    recur_interval: u64,
) -> Result<()> {
    println!(
        "lt-runner serve: polling {} (interval={interval}s, once={once}, recur_interval={recur_interval}s)",
        cli.base
    );
    let mut last_sweep: Option<Instant> = None;
    loop {
        // Opt-in benchmark recurrence: on a subsampled cadence (and always on the first iteration /
        // `--once`), enqueue a bench_run for any recurring benchmark that is due. A sweep failure is
        // non-fatal — like the dataset scheduler, a transient API blip must not kill the worker.
        if recur_interval > 0 && sweep_due(last_sweep, recur_interval) {
            if let Err(e) = recurrence::check_and_enqueue(cli, http) {
                eprintln!("recurrence sweep error (continuing): {e}");
            }
            last_sweep = Some(Instant::now());
        }
        match claim(cli, http, stale_secs)? {
            Some(job) => {
                println!(
                    "claimed job {} type={} (attempt {}/{}, failures {}, worker deaths {})",
                    short(&job.id),
                    job.job_type,
                    job.attempts,
                    job.max_attempts,
                    job.failures,
                    job.stale_reclaims,
                );
                run_claimed_job(cli, http, engine, &job)?;
            }
            None => {
                if !once {
                    std::thread::sleep(Duration::from_secs(interval));
                }
            }
        }
        if once {
            break;
        }
    }
    Ok(())
}

/// Execute one claimed job with a cancel watcher alongside it, then finish it honestly.
///
/// The retry decision uses `failures` — runs that actually failed — not `attempts`, which the claim
/// bumps for a crashed worker too. Three crashes used to permanently fail a job and record the crash
/// as its error, hiding whether the benchmark had ever failed at all.
fn run_claimed_job(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    job: &Job,
) -> Result<()> {
    let ctl = RunControl::for_job(cli, http, &job.id);
    ctl.note("starting");
    let finished = AtomicBool::new(false);

    let outcome = std::thread::scope(|scope| {
        // Watcher: ask the API whether an operator cancelled this job. The run itself notices at its
        // next case boundary — nothing is ever interrupted mid-call.
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(CANCEL_POLL_INTERVAL);
                if finished.load(Ordering::Relaxed) {
                    break;
                }
                if ctl.poll_cancelled() {
                    eprintln!("  cancel requested — stopping at the next case boundary");
                    ctl.cancel();
                    break;
                }
            }
        });
        let outcome = process_job(cli, http, engine, job, &ctl);
        finished.store(true, Ordering::Relaxed);
        outcome
    });

    match outcome {
        Ok(result) => {
            // A cancelled run's partial results are already recorded (and marked partial) by the
            // benchmark itself; the job says so too, and carries no error — cancelling is not a
            // failure, so it must not consume a retry.
            let status = if ctl.cancelled() { "cancelled" } else { "done" };
            finish(cli, http, &job.id, status, &result, None)?;
            println!("  -> {status}");
        }
        Err(e) => {
            let (status, note) = retry_decision(job.failures, job.max_attempts);
            let error = format!("{JOB_ERROR_PREFIX_FAILURE}{e}");
            finish(cli, http, &job.id, status, &Value::Null, Some(&error))?;
            eprintln!("  -> {status} ({note}): {e}");
        }
    }
    Ok(())
}

/// Whether a *reported* failure retries, given how many real failures the job has already had.
/// Returns the next status and a human note. Pure, so the accounting is testable.
fn retry_decision(failures: u32, max_attempts: u32) -> (&'static str, String) {
    // `failures` is the count BEFORE this one; finishing with an error records it.
    let after = failures + 1;
    if after < max_attempts {
        ("queued", format!("failure {after}/{max_attempts}, retrying"))
    } else {
        ("failed", format!("failure {after}/{max_attempts}, giving up"))
    }
}

/// Whether a recurrence sweep is due: always on the first iteration (`None`), then no more often than
/// `recur_interval`. Subsampling keeps the sweep off the hot 5s claim loop.
fn sweep_due(last_sweep: Option<Instant>, recur_interval: u64) -> bool {
    match last_sweep {
        None => true,
        Some(t) => t.elapsed() >= Duration::from_secs(recur_interval),
    }
}

fn claim(cli: &Cli, http: &reqwest::blocking::Client, stale_secs: i64) -> Result<Option<Job>> {
    let v = post(cli, http, "/v1/jobs/claim", &json!({ "stale_secs": stale_secs }))?;
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_value(v)?))
    }
}

fn finish(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    id: &str,
    status: &str,
    result: &Value,
    error: Option<&str>,
) -> Result<()> {
    post(
        cli,
        http,
        &format!("/v1/jobs/{id}/finish"),
        &json!({ "status": status, "result": result, "error": error }),
    )?;
    Ok(())
}

fn process_job(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    job: &Job,
    ctl: &RunControl,
) -> Result<Value> {
    match job.job_type.as_str() {
        "bench_run" => {
            let bid = job
                .payload
                .get("benchmark_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("bench_run payload missing benchmark_id"))?;
            let samples = job.payload.get("samples").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            let gen_samples =
                job.payload.get("gen_samples").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            let heal = job.payload.get("heal").and_then(|v| v.as_bool()).unwrap_or(false);
            let pairwise = job.payload.get("pairwise").and_then(|v| v.as_bool()).unwrap_or(false);
            // Bounded parallelism for queued bench jobs; defaults to the CLI's --jobs (4).
            let jobs = job.payload.get("jobs").and_then(|v| v.as_u64()).unwrap_or(cli.jobs as u64) as usize;
            ctl.note(&format!("running benchmark {bid}"));
            // Provenance passthrough: a version-triggered enqueue (prompts::maybe_enqueue) tags its
            // job payload with the prompt + version being scored; stamp them into the run report so
            // the promotion gate can find the run that scored THAT version.
            let extra = {
                let mut m = serde_json::Map::new();
                if let Some(pid) = job.payload.get("prompt_id").filter(|v| !v.is_null()) {
                    m.insert("prompt_id".into(), pid.clone());
                }
                if let Some(v) = job.payload.get("version").filter(|v| !v.is_null()) {
                    m.insert("prompt_version".into(), v.clone());
                }
                (!m.is_empty()).then_some(Value::Object(m))
            };
            let status = run_benchmark(
                cli, http, engine, bid, samples, gen_samples, heal, pairwise, jobs,
                extra.as_ref(), ctl,
            )?;
            Ok(json!({
                "benchmark_id": bid,
                "status": status,
                "cancelled": ctl.cancelled(),
                "partial": ctl.cancelled(),
            }))
        }
        other => Err(anyhow::anyhow!("unknown job type: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::retry_decision;

    #[test]
    fn only_reported_failures_consume_the_retry_budget() {
        // First real failure of three → retry.
        assert_eq!(retry_decision(0, 3).0, "queued");
        assert_eq!(retry_decision(1, 3).0, "queued");
        // Third → give up, and say which failure it was.
        let (status, note) = retry_decision(2, 3);
        assert_eq!(status, "failed");
        assert!(note.contains("3/3"), "{note}");
        // The regression this replaces: a job whose worker was killed twice (attempts=3) but which
        // never actually failed still gets its full retry budget, because `failures` is what counts.
        assert_eq!(retry_decision(0, 3).0, "queued");
    }
}
