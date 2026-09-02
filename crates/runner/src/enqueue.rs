//! `--via-queue`: run a cycle *through the job queue* instead of in this process.
//!
//! The same work either way — the arm in [`crate::dispatch`] is what executes it. What the queue
//! adds is everything a bare `--interval` daemon never had: a lease with a heartbeat (so a killed
//! worker is noticed in minutes and its work reclaimed rather than silently lost), cancellation,
//! honest retry accounting, live progress, and a durable record that the cycle ran at all.
//!
//! Enqueue-then-serve-once, so `--via-queue` is still a foreground command that finishes: it posts
//! the job, then claims exactly that kind and runs one job. Recurrence is deliberately NOT here —
//! that is a stored `Schedule` (`POST /v1/projects/:id/schedules`), swept by the API.

use anyhow::{bail, Result};
use serde_json::Value;

use lighttrack_core::{Job, JobKind};
use lighttrack_engine::EngineConfig;

use crate::cli::Cli;
use crate::http::post;
use crate::serve::{self, ServeParams};
use crate::util::short;

/// Enqueue one job of `kind` and immediately serve a single job of that kind.
pub(crate) fn run_via_queue(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    kind: JobKind,
    payload: Value,
) -> Result<()> {
    let v = post(
        cli,
        http,
        "/v1/jobs",
        &serde_json::json!({ "type": kind.as_str(), "payload": payload }),
    )?;
    let job: Job = serde_json::from_value(v)?;
    println!(
        "enqueued {} job {} — serving it here (--via-queue)",
        kind.as_str(),
        short(&job.id)
    );
    // `--kinds` narrowed to this one so a shared queue's other work is not claimed by what the
    // operator asked to be a one-shot command.
    let params = ServeParams {
        once: true,
        interval: 0,
        stale_secs: 120,
        lease_renew_secs: 0,
        kinds: vec![kind.as_str().to_string()],
        providers: serve::providers_from_env(),
    };
    serve::serve(cli, http, engine, &params)?;
    Ok(())
}

/// The `--rubric` / `--rubric-id` pair as payload fields, refused here if neither is given — the
/// same contract `Judge::resolve` enforces, applied before anything is enqueued.
pub(crate) fn judge_fields(
    payload: &mut serde_json::Map<String, Value>,
    rubric: Option<&str>,
    rubric_id: Option<&str>,
) -> Result<()> {
    match (rubric, rubric_id) {
        (Some(t), None) => {
            payload.insert("rubric".into(), Value::String(t.to_string()));
        }
        (None, Some(id)) => {
            payload.insert("rubric_id".into(), Value::String(id.to_string()));
        }
        (Some(_), Some(_)) => bail!("pass exactly one of --rubric or --rubric-id, not both"),
        (None, None) => bail!("one of --rubric or --rubric-id is required"),
    }
    Ok(())
}
