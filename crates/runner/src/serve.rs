//! `serve`: the job-queue worker loop — claim a job, run it, finish it (with retry up to max_attempts).

use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::Job;
use lighttrack_engine::EngineConfig;

use crate::bench::run_benchmark;
use crate::cli::Cli;
use crate::http::post;
use crate::recurrence;
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
                    "claimed job {} type={} (attempt {}/{})",
                    short(&job.id),
                    job.job_type,
                    job.attempts,
                    job.max_attempts
                );
                match process_job(cli, http, engine, &job) {
                    Ok(result) => {
                        finish(cli, http, &job.id, "done", &result, None)?;
                        println!("  -> done");
                    }
                    Err(e) => {
                        let status = if job.attempts < job.max_attempts {
                            "queued" // retry
                        } else {
                            "failed"
                        };
                        finish(cli, http, &job.id, status, &Value::Null, Some(&e.to_string()))?;
                        eprintln!("  -> {status}: {e}");
                    }
                }
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
            let _ = post(
                cli,
                http,
                &format!("/v1/jobs/{}/progress", job.id),
                &json!({ "progress": format!("running benchmark {bid}") }),
            );
            run_benchmark(cli, http, engine, bid, samples, gen_samples, heal, pairwise, jobs)?;
            Ok(json!({ "benchmark_id": bid, "status": "completed" }))
        }
        other => Err(anyhow::anyhow!("unknown job type: {other}")),
    }
}
