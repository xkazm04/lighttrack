//! Enqueueing work: the benchmark door, the generic `POST /v1/jobs` door, and the one constructor
//! they and the schedule sweep all mint rows through.
//!
//! Kept apart from the worker-facing half of the queue ([`crate::jobs`]) because it is where the
//! *typing* lives: a payload is checked against its [`JobKind`] here, at the door, instead of being
//! discovered as unparseable by the worker that claimed it — which used to cost three claims and a
//! dead-lettered job to report a typo.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{new_id, validate_payload, Job, JobKind};

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
        Some(&bench.project_id),
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
    project: Option<&str>,
    benchmark_id: &str,
    extra: serde_json::Value,
) -> Result<Job, ApiError> {
    let mut payload = serde_json::json!({ "benchmark_id": benchmark_id });
    if let (Some(obj), Some(into)) = (extra.as_object(), payload.as_object_mut()) {
        for (k, v) in obj {
            into.insert(k.clone(), v.clone());
        }
    }
    enqueue(st, project, JobKind::BenchRun, payload).await
}

/// Enqueue one job of `kind`, **payload-validated first**.
///
/// Validation at the door, not at the worker, is the point: an unparseable payload used to be
/// discovered by the worker that claimed it, which then failed, retried, failed and dead-lettered —
/// three claims and a dead job to report a typo. The one shared constructor also means every
/// producer (the benchmark route, `POST /v1/jobs`, the schedule sweep) mints the same row.
///
/// `project` is the tenant the work belongs to, stamped onto the row so `GET /v1/jobs` can be read
/// under a scope at all (M17). `None` is an operator job — one this deployment enqueued for itself
/// rather than for a tenant — and only an operator scope reads those back.
pub(crate) async fn enqueue(
    st: &AppState,
    project: Option<&str>,
    kind: JobKind,
    payload: serde_json::Value,
) -> Result<Job, ApiError> {
    validate_payload(kind, &payload).map_err(ApiError::bad_request)?;
    let job = Job {
        id: new_id(),
        job_type: kind.as_str().to_string(),
        payload,
        status: "queued".to_string(),
        project_id: project.map(str::to_string),
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
pub(crate) struct EnqueueJobReq {
    #[serde(rename = "type")]
    job_type: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// `POST /v1/jobs` — enqueue any kind of background work, not only a benchmark run.
///
/// The door an external scheduler (cron, Cloud Scheduler) uses when a deployment would rather drive
/// recurrence itself than store it. An unknown `type` is a 400 naming the vocabulary, because the
/// alternative — accepting it — produces a job every worker refuses and no operator can explain.
pub(crate) async fn enqueue_job(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnqueueJobReq>,
) -> Result<Json<Job>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let kind = parse_kind(&req.job_type)?;
    // An admin-enqueued job with no project of its own is an operator job.
    Ok(Json(
        enqueue(&st, p.scope().project(), kind, req.payload).await?,
    ))
}

/// Parse a job-kind literal, or a 400 that names every kind this build knows.
pub(crate) fn parse_kind(s: &str) -> Result<JobKind, ApiError> {
    JobKind::from_wire(s).ok_or_else(|| {
        ApiError::bad_request(format!(
            "unknown job type {s:?}: expected {}",
            JobKind::vocabulary()
        ))
    })
}
