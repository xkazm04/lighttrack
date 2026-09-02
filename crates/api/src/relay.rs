//! Cloud→device relay queue (docs/RELAY.md) — apps enqueue an `action_type` + JSON params; the
//! enrolled local device leases due tasks over outbound HTTPS (no inbound connectivity to the
//! device), executes them against its local action library with the Claude Code CLI, and reports
//! the outcome. The payload carries parameters only: prompts, allowed tools, and connector
//! credentials never transit the cloud.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::{
    new_id, RelayAdmission, RelayStatus, RelayTask, RELAY_DEFAULT_MAX_ATTEMPTS,
    RELAY_DEFAULT_RETRY_INTERVAL_SECS,
};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct EnqueueReq {
    action_type: String,
    #[serde(default)]
    payload: Value,
    /// Admin/dev only; a project key forces its own project.
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    max_attempts: Option<u32>,
    retry_interval_secs: Option<u32>,
}

/// An accepted enqueue: the task, plus **why it was accepted**.
///
/// The task is flattened so every existing caller (the SDKs read `id`/`status` straight off the
/// body) keeps working unchanged, and `admission` is the new field beside it.
#[derive(serde::Serialize)]
pub(crate) struct EnqueueResp {
    #[serde(flatten)]
    task: RelayTask,
    /// Always `queued { eligible_devices }` here — a refusal never reaches this shape, it is a 422.
    /// Carried anyway because `eligible_devices: 1` and `eligible_devices: 6` are very different
    /// things to have just enqueued against.
    admission: RelayAdmission,
}

/// Ask the fleet whether anything could run this action type.
///
/// Two answers are deliberately identical here: a backend that does not serve the device fleet, and
/// a deployment with nobody enrolled. Both are "there is nothing to route against", which admits —
/// the legacy shared-key relay is exactly that shape, and refusing its traffic would be this
/// feature breaking the thing it hardens.
async fn admit(st: &AppState, action_type: &str) -> RelayAdmission {
    let store = st.store.clone();
    let at = action_type.to_string();
    match spawn_db(move || store.count_eligible_devices(&at)).await {
        Ok(e) => e.admit(action_type),
        Err(e) => {
            tracing::debug!(error = %e, "relay enqueue: device fleet unavailable; admitting");
            RelayAdmission::Queued {
                eligible_devices: 0,
            }
        }
    }
}

pub(crate) async fn enqueue_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnqueueReq>,
) -> Result<Json<EnqueueResp>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_ingest_project(&p, &req.project_id)?;
    if req.action_type.trim().is_empty() {
        return Err(ApiError::bad_request("action_type is required"));
    }
    // Idempotent enqueue: the same (project, key) returns the existing task instead of a duplicate.
    // Checked BEFORE admission, so a re-submitted key keeps answering with the task that exists
    // even if the fleet has since changed shape — idempotency is about the record, not the roster.
    if let Some(key) = req.idempotency_key.clone() {
        let store = st.store.clone();
        let project2 = project.clone();
        if let Some(existing) =
            spawn_db(move || store.find_relay_task_by_key(&project2, &key)).await?
        {
            let admission = admit(&st, &existing.action_type).await;
            return Ok(Json(EnqueueResp {
                task: existing,
                admission,
            }));
        }
    }
    // Admission (M18). Validation used to be "action_type is non-empty", so a typo was
    // indistinguishable from a healthy backlog: the task sat queued, was handed to devices that had
    // no such action, burned every attempt on "no action", and dead-lettered hours later. A 422
    // here costs the caller one round trip and names the fix.
    let admission = admit(&st, req.action_type.trim()).await;
    if let RelayAdmission::Refused { reason } = &admission {
        return Err(ApiError::relay_unroutable(reason.clone()));
    }
    let now = Utc::now();
    let task = RelayTask {
        id: new_id(),
        project_id: project,
        source: req.source,
        action_type: req.action_type,
        payload: req.payload,
        status: RelayStatus::Queued.as_str().to_string(),
        attempts: 0,
        failures: 0,
        stale_reclaims: 0,
        lease_fence: None,
        progress: None,
        max_attempts: req
            .max_attempts
            .unwrap_or(RELAY_DEFAULT_MAX_ATTEMPTS)
            .max(1),
        retry_interval_secs: req
            .retry_interval_secs
            .unwrap_or(RELAY_DEFAULT_RETRY_INTERVAL_SECS),
        idempotency_key: req.idempotency_key,
        device: None,
        lease_deadline: None,
        next_attempt_at: now,
        result: Value::Null,
        error: None,
        created_at: now,
        updated_at: now,
    };
    let store = st.store.clone();
    let t2 = task.clone();
    spawn_db(move || store.create_relay_task(&t2)).await?;
    Ok(Json(EnqueueResp { task, admission }))
}

pub(crate) async fn get_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<RelayTask>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let task = spawn_db(move || store.get_relay_task(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("relay task '{id}' not found")))?;
    if let Principal::Project {
        project_id: pid, ..
    } = &p
    {
        if *pid != task.project_id {
            return Err(ApiError::forbidden("key not authorized for that project"));
        }
    }
    Ok(Json(task))
}

#[derive(Deserialize)]
pub(crate) struct ListParams {
    project: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn list_tasks(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListParams>,
) -> Result<Json<Vec<RelayTask>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    // Validate the status filter against the task-status authority, so a plausible-but-wrong term
    // (e.g. `?status=failed`, a settle-vocabulary word that is not a task status) is a 400 rather
    // than a silently-empty page — "no dead tasks" and "you filtered on a non-status" must differ.
    if let Some(s) = q.status.as_deref() {
        if RelayStatus::from_wire(s).is_none() {
            let expected = RelayStatus::ALL
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            return Err(ApiError::bad_request(format!(
                "invalid 'status' {s:?}: expected {expected}"
            )));
        }
    }
    let store = st.store.clone();
    let status = q.status;
    let limit = q.limit.unwrap_or(50).min(1000);
    let tasks =
        spawn_db(move || store.list_relay_tasks(project.as_deref(), status.as_deref(), limit))
            .await?;
    Ok(Json(tasks))
}
