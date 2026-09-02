//! What a claimed job *is*: the dispatch from a [`JobKind`] to the code that runs one cycle of it.
//!
//! Before M7 this was a `match job.job_type.as_str()` with one arm, and the other four workloads
//! shipped as separately-scheduled daemon loops (`score --interval`, `score-traces --interval`,
//! `schedule --interval`, `calibrate --watch`). Everything the queue provides — a lease, a
//! heartbeat, cancellation, honest retry accounting, progress an operator can read, a record that
//! the work ran at all — protected exactly one of the five. Routing them all through here is what
//! makes that machinery apply to all of them: each arm runs **one cycle** and returns, and the
//! recurrence that used to live in a `--interval` flag now lives in a stored `Schedule`.
//!
//! Every arm inherits the same [`RunControl`], so cancellation and progress work identically
//! whichever kind is running.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use lighttrack_core::{
    BenchRunPayload, CalibratePayload, ContributePayload, DatasetSamplePayload, Job, JobKind,
    ScoreEventsPayload, ScoreTracesPayload,
};
use lighttrack_engine::EngineConfig;

use crate::bench::run_benchmark;
use crate::cli::Cli;
use crate::judge_spec::Judge;
use crate::runctl::RunControl;
use crate::{calibrate_watch, schedule, score, score_traces};

pub(crate) fn process_job(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    job: &Job,
    ctl: &RunControl,
) -> Result<Value> {
    let kind = job.kind().ok_or_else(|| {
        anyhow!(
            "unknown job type '{}': this worker knows {}. A newer API may have enqueued a kind \
             this build cannot run — upgrade the worker, or declare `serve --kinds` so it stops \
             claiming what it cannot execute",
            job.job_type,
            JobKind::vocabulary()
        )
    })?;
    // Parse ONCE, at the top, into the kind's declared shape. A payload that cannot be parsed is a
    // failure before any paid call goes out, not halfway through one.
    let p = &job.payload;
    match kind {
        JobKind::BenchRun => bench_run(cli, http, engine, parse(p)?, ctl),
        JobKind::ScoreEvents => score_events(cli, http, engine, parse(p)?, ctl),
        JobKind::ScoreTraces => score_traces(cli, http, engine, parse(p)?, ctl),
        JobKind::DatasetSample => dataset_sample(cli, http, engine, parse(p)?, ctl),
        JobKind::Calibrate => calibrate(cli, http, engine, parse(p)?, ctl),
        JobKind::Contribute => contribute(cli, http, parse(p)?, ctl),
    }
}

fn parse<T: for<'de> serde::Deserialize<'de>>(payload: &Value) -> Result<T> {
    serde_json::from_value(payload.clone()).map_err(|e| anyhow!("bad job payload: {e}"))
}

fn bench_run(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: BenchRunPayload,
    ctl: &RunControl,
) -> Result<Value> {
    ctl.note(&format!("running benchmark {}", p.benchmark_id));
    // Provenance passthrough: a version-triggered enqueue (prompts::maybe_enqueue) tags its job
    // payload with the prompt + version being scored; stamp them into the run report so the
    // promotion gate can find the run that scored THAT version.
    let mut extra = serde_json::Map::new();
    if let Some(id) = &p.prompt_id {
        extra.insert("prompt_id".into(), json!(id));
    }
    if let Some(v) = p.version {
        extra.insert("prompt_version".into(), json!(v));
    }
    // The registry NAME, not just the id: it is the key a target's `prompt_ref` matches on, so it
    // is what tells the resolver which target of a matrix this version overrides.
    if let Some(n) = &p.prompt_name {
        extra.insert("prompt_name".into(), json!(n));
    }
    let extra = (!extra.is_empty()).then_some(Value::Object(extra));
    let status = run_benchmark(
        cli,
        http,
        engine,
        &p.benchmark_id,
        p.samples,
        p.gen_samples,
        // Queued runs judge unbatched. These are the runs most likely to be compared against a
        // stored baseline, and batching is a methodology change — opting a queue into it silently
        // would make a gate verdict mean something different without anyone asking.
        1,
        p.heal,
        p.pairwise,
        p.jobs.unwrap_or(cli.jobs),
        extra.as_ref(),
        ctl,
    )?;
    Ok(json!({
        "benchmark_id": p.benchmark_id,
        "status": status,
        "cancelled": ctl.cancelled(),
        "partial": ctl.cancelled(),
    }))
}

fn score_events(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: ScoreEventsPayload,
    ctl: &RunControl,
) -> Result<Value> {
    let judge = Judge::resolve(
        cli,
        http,
        p.judge.rubric.as_deref(),
        p.judge.rubric_id.as_deref(),
    )?;
    ctl.note("scoring recent events");
    // `interval = 0` is the whole adaptation: one cycle, then return. The daemon loop this
    // replaces is now the schedule row that enqueued this job.
    score::score_recent(
        cli,
        http,
        engine,
        &judge,
        p.project.as_deref(),
        p.prompt_tag.as_deref(),
        p.limit,
        0,
        cli.jobs,
    )?;
    Ok(json!({ "kind": "score_events", "project": p.project, "limit": p.limit }))
}

fn score_traces(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: ScoreTracesPayload,
    ctl: &RunControl,
) -> Result<Value> {
    // Per-job judge override, else the worker's global --model. Built without cloning EngineConfig,
    // the same way the CLI path does it.
    let eng = EngineConfig {
        claude_bin: engine.claude_bin.clone(),
        model: p
            .judge_model
            .clone()
            .unwrap_or_else(|| engine.model.clone()),
        bare: engine.bare,
    };
    ctl.note(&format!("scoring traces for {}", p.project));
    let params = score_traces::Params {
        project: &p.project,
        rubric_text: p.judge.rubric.as_deref(),
        rubric_id: p.judge.rubric_id.as_deref(),
        sample_every: p.sample_every,
        errors_always: p.errors_always,
        settle_secs: p.settle_secs,
        limit: p.limit,
        interval: 0,
        once: true,
        jobs: cli.jobs,
    };
    score_traces::run(cli, http, &eng, &params)?;
    Ok(json!({ "kind": "score_traces", "project": p.project }))
}

fn dataset_sample(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: DatasetSamplePayload,
    ctl: &RunControl,
) -> Result<Value> {
    ctl.note(&format!("sampling events from {}", p.project));
    let built = schedule::run_cycle(
        cli,
        http,
        engine,
        &p.project,
        p.n,
        &p.name_prefix,
        p.llm_scrub,
    )?;
    // A skipped cycle is a SUCCESS, not a failure: naming each dataset after its watermark makes
    // sampling idempotent, so "this window was already captured" is the mechanism working.
    Ok(json!({ "kind": "dataset_sample", "project": p.project, "dataset": built }))
}

fn calibrate(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: CalibratePayload,
    ctl: &RunControl,
) -> Result<Value> {
    let set = crate::calibrate::load_set(cli, http, p.file.as_deref(), p.dataset_id.as_deref())?;
    ctl.note(&format!("calibrating against {}", set.source));
    let params = calibrate_watch::WatchParams {
        set: &set,
        rubric_text: p.judge.rubric.as_deref(),
        rubric_id: p.judge.rubric_id.as_deref(),
        project: p.project.as_deref(),
        threshold: p.threshold,
        kappa_bar: p.kappa_bar,
        drift_threshold: p.drift_threshold,
        samples: p.samples,
        interval: 0,
        once: true,
        jobs: cli.jobs,
    };
    let code = calibrate_watch::watch(cli, http, engine, &params)?;
    // An untrusted judge is a RESULT, not a job failure: the cycle ran and measured exactly what it
    // set out to. Failing the job would burn the retry budget re-measuring a judge that is simply
    // no longer trustworthy, and would hide the verdict behind an error string.
    Ok(json!({
        "kind": "calibrate",
        "source": set.source,
        "trusted": code == 0,
        "exit_code": code,
    }))
}

/// One cycle of the collective auto-push: ask the API to contribute, and report what it decided.
///
/// The worker deliberately does **no** collective logic of its own — no digest build, no hash, no
/// outbound call to the hub. `POST /v1/collective/contribute` owns all of it, which is what keeps
/// the scheduled push and the hand-run `lt collective contribute` byte-identical; a second
/// implementation here is exactly how the hash gate would come to compare two digests that were
/// never the same object.
///
/// A `skipped` cycle is a **success**, not a failure: the gate deciding nothing changed is the
/// mechanism working, and failing the job would burn the retry budget re-deciding it.
fn contribute(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    p: ContributePayload,
    ctl: &RunControl,
) -> Result<Value> {
    p.validate().map_err(|e| anyhow!("{e}"))?;
    ctl.note("contributing this instance's digest");
    let mut body = json!({ "hub": p.hub });
    if let Some(r) = &p.hub_key_ref {
        body["hub_key_ref"] = json!(r);
    }
    if let Some(m) = p.min_cases {
        body["min_cases"] = json!(m);
    }
    let ack = crate::http::post(cli, http, "/v1/collective/contribute", &body)?;
    let outcome = ack
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    ctl.note(&format!("contribution {outcome}"));
    Ok(json!({ "kind": "contribute", "outcome": outcome, "ack": ack }))
}
