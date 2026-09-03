//! Job queue (Phase 3.6d) — the worker-facing half: claim, progress, renew, finish, and the
//! operator's cancel/read. Enqueue lives in [`crate::jobs_enqueue`].

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{Job, JobCancel, JobFinish};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::jobs_enqueue::parse_kind;
use crate::state::{spawn_db, AppState};

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
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let status = q.status;
    let limit = q.limit.unwrap_or(50).min(1000);
    // Scoped even though the door is admin-only today: the queue carries other projects' payloads,
    // so the read must be safe on its own terms rather than by whoever guards the route (M17).
    let sc = p.scope_owned();
    let jobs =
        spawn_db(move || store.list_jobs(sc.as_deref().into(), status.as_deref(), limit)).await?;
    Ok(Json(jobs))
}

pub(crate) async fn get_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    let job = spawn_db(move || store.get_job(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job '{id}' not found")))?;
    Ok(Json(job))
}

#[derive(Deserialize)]
pub(crate) struct ClaimReq {
    #[serde(default = "default_stale_secs")]
    stale_secs: i64,
    /// The kinds this worker can execute. Empty (or absent — what an older runner sends) means
    /// "any kind", which is what every worker meant while `bench_run` was the only one.
    #[serde(default)]
    kinds: Vec<String>,
    /// Which model providers this worker has credentials for. Advisory today: it is recorded in the
    /// claim log so an operator can see *why* a queue is not draining, but it does not filter,
    /// because a job's provider is a property of the benchmark/rubric it names rather than of the
    /// job row — filtering on it would need a read per candidate inside the atomic claim. Declared
    /// now so a worker fleet's capabilities are in one place when M18 gives it teeth.
    #[serde(default)]
    providers: Vec<String>,
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

/// The shortest stale window a claim may ask for. A worker renews every `stale / 3` seconds, so
/// below this a live holder cannot heartbeat fast enough to keep its own job — and `0`, which the
/// request shape allowed, reclaimed EVERY running job on the instance at once, so a misconfigured
/// second runner re-ran (and re-paid for) work its colleague was mid-way through.
const MIN_STALE_SECS: i64 = 10;

pub(crate) async fn claim_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ClaimReq>,
) -> Result<Json<Option<Job>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    if req.stale_secs < MIN_STALE_SECS {
        tracing::warn!(
            requested = req.stale_secs,
            floor = MIN_STALE_SECS,
            "worker asked for a stale window below the floor; clamped"
        );
    }
    let stale_before = Utc::now() - chrono::Duration::seconds(req.stale_secs.max(MIN_STALE_SECS));
    // Refuse an unknown kind rather than silently dropping it from the filter: a typo'd declaration
    // would otherwise narrow the worker to the kinds it spelled correctly, and it would look like
    // an empty queue.
    for k in &req.kinds {
        parse_kind(k)?;
    }
    if !req.providers.is_empty() {
        tracing::debug!(providers = ?req.providers, kinds = ?req.kinds, "worker claim");
    }
    let store = st.store.clone();
    let job = spawn_db(move || {
        let kinds: Vec<&str> = req.kinds.iter().map(String::as_str).collect();
        store.claim_job(stale_before, &kinds)
    })
    .await?;
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
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    let outcome = spawn_db(move || store.cancel_job(sc.as_deref().into(), &id2))
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
