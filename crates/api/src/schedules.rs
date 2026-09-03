//! Stored schedules: CRUD over the recurring workloads this deployment runs.
//!
//! The surface that answers "what runs on a schedule here" — a question that previously required
//! reading five daemons' command lines and one benchmark field that compare benchmarks could not
//! carry. Writing a schedule validates its payload against its kind immediately, so a schedule that
//! could only ever enqueue rejected jobs is refused when it is created rather than every interval,
//! forever, in a log nobody reads.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::{new_id, validate_payload, Job, Schedule, SCHEDULE_MIN_INTERVAL_SECS};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::jobs_enqueue::parse_kind;
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

#[derive(Deserialize)]
pub(crate) struct CreateReq {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
    interval_secs: u32,
    /// Seconds until the first firing. Default 0 — due at once, which is what someone who just
    /// wrote a schedule almost always means; an operator who wants the first run tomorrow says so.
    #[serde(default)]
    start_in_secs: i64,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

pub(crate) async fn create_schedule(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(req): Json<CreateReq>,
) -> Result<Json<Schedule>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let kind = parse_kind(&req.kind)?;
    validate_payload(kind, &req.payload).map_err(ApiError::bad_request)?;
    let now = Utc::now();
    let s = Schedule {
        id: new_id(),
        project_id: project,
        kind: kind.as_str().to_string(),
        payload: req.payload,
        interval_secs: req.interval_secs.max(SCHEDULE_MIN_INTERVAL_SECS),
        next_due: now + Duration::seconds(req.start_in_secs.max(0)),
        last_job_id: None,
        enabled: req.enabled,
        created_at: now,
    };
    let store = st.store.clone();
    let s2 = s.clone();
    spawn_db(move || store.create_schedule(&s2)).await?;
    Ok(Json(s))
}

pub(crate) async fn list_schedules(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> Result<Json<Vec<Schedule>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    // Reuses the read guard so a project key sees only its own schedules, and a mismatched one is a
    // 403 rather than an empty list.
    let project = resolve_read_project(&p, Some(&project))?.unwrap_or_default();
    let store = st.store.clone();
    Ok(Json(
        spawn_db(move || store.list_schedules(&project)).await?,
    ))
}

/// Every recurring workload in the deployment, across projects (admin).
///
/// The listing the design set out to make possible: one request that names every schedule, instead
/// of an operator reconstructing the answer from process arguments.
pub(crate) async fn list_all_schedules(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Schedule>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let all = spawn_db(move || {
        let mut out = Vec::new();
        for p in store.list_projects()? {
            out.extend(store.list_schedules(&p.id)?);
        }
        Ok(out)
    })
    .await?;
    Ok(Json(all))
}

#[derive(Deserialize)]
pub(crate) struct UpdateReq {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    interval_secs: Option<u32>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Patch a schedule. Every field is optional and absent means "leave it", so pausing one
/// (`{"enabled": false}`) cannot accidentally rewrite its payload — and pausing is deliberately
/// distinct from deleting: an operator has to be able to see the thing they paused.
pub(crate) async fn update_schedule(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdateReq>,
) -> Result<Json<Schedule>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let mut s = load(&st, &id).await?;
    if let Some(k) = req.kind.as_deref() {
        s.kind = parse_kind(k)?.as_str().to_string();
    }
    if let Some(p) = req.payload {
        s.payload = p;
    }
    if let Some(i) = req.interval_secs {
        s.interval_secs = i.max(SCHEDULE_MIN_INTERVAL_SECS);
    }
    if let Some(e) = req.enabled {
        s.enabled = e;
    }
    // Re-validate the pair, not just the changed half: a payload edit and a kind edit each make the
    // other's stored value potentially wrong.
    let kind = parse_kind(&s.kind)?;
    validate_payload(kind, &s.payload).map_err(ApiError::bad_request)?;
    let store = st.store.clone();
    let s2 = s.clone();
    if !spawn_db(move || store.update_schedule(TenantScope::Operator, &s2)).await? {
        return Err(ApiError::not_found(format!("schedule '{id}' not found")));
    }
    Ok(Json(s))
}

pub(crate) async fn delete_schedule(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    if !spawn_db(move || store.delete_schedule(sc.as_deref().into(), &id2)).await? {
        return Err(ApiError::not_found(format!("schedule '{id}' not found")));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}

/// The jobs this schedule has produced, newest first.
///
/// Filtered from the job list by the `schedule_id` the sweep stamps into every payload it enqueues,
/// rather than by a foreign key: a job outlives the schedule that made it (deleting a schedule must
/// not orphan or erase its history), and the queue's row deliberately has no per-producer column.
pub(crate) async fn schedule_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<Job>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let sc = p.scope_owned();
    let jobs = spawn_db(move || store.list_jobs(sc.as_deref().into(), None, 1000)).await?;
    Ok(Json(
        jobs.into_iter()
            .filter(|j| j.payload.get(SCHEDULE_ID_KEY).and_then(Value::as_str) == Some(&id2))
            .collect(),
    ))
}

/// The payload key the sweep stamps so a job can name the schedule that produced it.
pub(crate) const SCHEDULE_ID_KEY: &str = "schedule_id";

async fn load(st: &AppState, id: &str) -> Result<Schedule, ApiError> {
    let store = st.store.clone();
    let id2 = id.to_string();
    spawn_db(move || store.get_schedule(TenantScope::Operator, &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("schedule '{id}' not found")))
}
