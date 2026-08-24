//! Job queue (Phase 3.6d) — enqueue returns immediately; `lt-runner serve` executes.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{new_id, Job, JobCancel, JobFinish};

use crate::benchmarks::load_benchmark_authorized;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct EnqueueReq {
    #[serde(default = "default_samples")]
    samples: u32,
    #[serde(default)]
    heal: bool,
}

fn default_samples() -> u32 {
    1
}

pub(crate) async fn enqueue_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<EnqueueReq>,
) -> Result<Json<Job>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let bench = load_benchmark_authorized(&st, &p, &id).await?;
    let job = enqueue_bench_run(
        &st,
        &bench.id,
        serde_json::json!({ "samples": req.samples, "heal": req.heal }),
    )
    .await?;
    Ok(Json(job))
}

/// Enqueue a `bench_run` job for a benchmark, merging `extra` payload keys (e.g. `samples`, `heal`,
/// or a `prompt_id`/`version` for traceability). Shared by the manual enqueue route and the prompt
/// registry's auto-enqueue on a new version.
pub(crate) async fn enqueue_bench_run(
    st: &AppState,
    benchmark_id: &str,
    extra: serde_json::Value,
) -> Result<Job, ApiError> {
    let mut payload = serde_json::json!({ "benchmark_id": benchmark_id });
    if let (Some(obj), Some(into)) = (extra.as_object(), payload.as_object_mut()) {
        for (k, v) in obj {
            into.insert(k.clone(), v.clone());
        }
    }
    let job = Job {
        id: new_id(),
        job_type: "bench_run".to_string(),
        payload,
        status: "queued".to_string(),
        attempts: 0,
        max_attempts: 3,
        failures: 0,
        stale_reclaims: 0,
        progress: None,
        error: None,
        result: serde_json::Value::Null,
        claimed_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let store = st.store.clone();
    let j2 = job.clone();
    spawn_db(move || store.create_job(&j2)).await?;
    Ok(job)
}

#[derive(Deserialize)]
pub(crate) struct JobsParams {
    status: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn list_jobs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<JobsParams>,
) -> Result<Json<Vec<Job>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let status = q.status;
    let limit = q.limit.unwrap_or(50).min(1000);
    let jobs = spawn_db(move || store.list_jobs(status.as_deref(), limit)).await?;
    Ok(Json(jobs))
}

pub(crate) async fn get_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let job = spawn_db(move || store.get_job(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job '{id}' not found")))?;
    Ok(Json(job))
}

#[derive(Deserialize)]
pub(crate) struct ClaimReq {
    #[serde(default = "default_stale_secs")]
    stale_secs: i64,
}

/// How long a dead worker may go unnoticed — **not** how long a job may legitimately run.
///
/// Those are two different quantities, and a single claim timestamp used to conflate them: this was
/// 600 s, chosen to be longer than a slow benchmark, which meant a killed worker's job sat
/// untouchable for ten minutes. Now that the holder renews its lease on a timer
/// (`/v1/jobs/:id/renew`), job duration is unbounded and irrelevant here, and this can be sized to
/// detection latency instead. 120 s is four missed 30 s heartbeats: a GC pause, a slow round trip,
/// or a brief network blip cannot cost a live worker its job, while a dead one is reclaimed in
/// about two minutes rather than ten.
fn default_stale_secs() -> i64 {
    120
}

pub(crate) async fn claim_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ClaimReq>,
) -> Result<Json<Option<Job>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let stale_before = Utc::now() - chrono::Duration::seconds(req.stale_secs.max(0));
    let store = st.store.clone();
    let job = spawn_db(move || store.claim_job(stale_before)).await?;
    Ok(Json(job))
}

/// Stop a queued or running benchmark job. A queued job is cancelled outright; a running one is
/// marked `cancelling` and its worker stops at the next case boundary, keeping (and marking) the
/// partial results it already produced. Cancelling a job that already finished is a **409**, not a
/// silent success: the operator needs to know their spend was not stopped by this call.
pub(crate) async fn cancel_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobCancel>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let outcome = spawn_db(move || store.cancel_job(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job '{id}' not found")))?;
    if let JobCancel::AlreadyFinished { status } = &outcome {
        return Err(ApiError::conflict(format!(
            "job '{id}' is already {status}; nothing was cancelled"
        )));
    }
    Ok(Json(outcome))
}

#[derive(Deserialize)]
pub(crate) struct ProgressReq {
    progress: String,
}

pub(crate) async fn job_progress(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ProgressReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    spawn_db(move || store.update_job_progress(&id, &req.progress)).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct RenewReq {
    /// The `claimed_at` the worker was handed at claim — its proof that the job is still its own.
    claimed_at: chrono::DateTime<Utc>,
}

/// Heartbeat: "I am still alive, extend my lease." A conditioned write, and its result is the
/// worker's own gate on its legitimacy — a **409** means the lease is no longer theirs (a reaper
/// expired it, an operator requeued the job, someone reclaimed it) and the run must stop rather
/// than keep spending as a zombie beside its successor.
///
/// The endpoint carries nothing but liveness on purpose. Progress rides `/progress`, so a stall in
/// computing progress can never stall the heartbeat and make a live-but-stuck worker read as a
/// dead one — the two states the lease exists to tell apart.
pub(crate) async fn job_renew(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<RenewReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let renewed = spawn_db(move || store.renew_job_lease(&id2, req.claimed_at)).await?;
    match renewed {
        Some(claimed_at) => Ok(Json(serde_json::json!({ "claimed_at": claimed_at }))),
        None => Err(ApiError::conflict(format!(
            "job '{id}' is no longer held by that lease; stop working on it"
        ))),
    }
}

#[derive(Deserialize)]
pub(crate) struct FinishReq {
    status: String,
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<String>,
    /// The lease the caller holds. A worker always sends it; omitting it is the operator-shaped
    /// finish, which waives the ownership condition but never the finality one.
    #[serde(default)]
    claimed_at: Option<chrono::DateTime<Utc>>,
}

/// Record a job's verdict. A **conditioned** write: the job must still be non-terminal, and — when
/// `claimed_at` is supplied — still held by this caller.
///
/// A refusal is a **409**, not a silent no-op. This closes the queue's last unfenced write: a worker
/// reclaimed as stale while it was busy would finish later and overwrite the verdict its
/// replacement had already recorded, plausibly and with nothing anywhere saying so. Now the slow
/// worker is told what beat it, and the verdict stands.
pub(crate) async fn job_finish(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<FinishReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let outcome = spawn_db(move || {
        store.finish_job(
            &id2,
            &req.status,
            &req.result,
            req.error.as_deref(),
            req.claimed_at,
        )
    })
    .await?;
    match outcome {
        JobFinish::Finished => Ok(Json(serde_json::json!({ "ok": true }))),
        JobFinish::NoSuchJob => Err(ApiError::not_found(format!("job '{id}' not found"))),
        JobFinish::NotHeld { status, claimed_at } => Err(ApiError::conflict(format!(
            "job '{id}' is {status} and not held by that lease (its lease is {claimed_at:?}); \
             the verdict you sent was NOT recorded"
        ))),
    }
}
