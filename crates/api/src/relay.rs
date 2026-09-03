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

use crate::error::ApiError;
use crate::events_admission::breach_reason;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use crate::ingest_proximity::Proximity;
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
    /// A soft-tier limit crossed its `warn_at` but nothing is enforcing yet. The task IS queued;
    /// this is the heads-up that the next few might not be. Absent when the project is clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
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

/// Whether the project's usage limits allow one more relay run to be queued.
///
/// **Why here.** A relay run is metered traffic — a headless `claude -p` bills at API rates (D0,
/// D18) — but nothing on this path checked a single cap. The settle-time event could not: by then
/// the run has happened, and refusing to *record* spend does not un-spend it. Enqueue is the last
/// moment a refusal is still free, so it is where admission belongs.
///
/// This is [`crate::limits::evaluate_project_limits`] in its read-only mode: the same evaluator, the
/// same thresholds, the same `basis` explanation the status page shows — so a caller cannot be told
/// two different stories about one cap. It costs one usage read per enqueue, which a queue measured
/// in tasks-per-minute can afford and the ingest path (measured in events-per-second) could not.
///
/// Returns `Ok(None)` when clear, `Ok(Some(warning))` for the soft tier, and `Err(429)` for a hard
/// breach. A limits backend that cannot answer admits: an unavailable evaluator is not evidence of
/// an exceeded budget, and refusing work on it would make a degraded read path an outage.
async fn budget_allows(st: &AppState, project: &str) -> Result<Option<String>, ApiError> {
    let statuses = match crate::limits::evaluate_project_limits(st, project).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(project_id = %project, error = %e, "relay enqueue: limits unavailable; admitting");
            return Ok(None);
        }
    };
    if statuses.iter().any(|s| s.rejects_ingest()) {
        let retry = statuses
            .iter()
            .filter(|s| s.rejects_ingest())
            .map(|s| s.retry_after_secs())
            .max();
        let mut prox = Proximity::of(&statuses);
        prox.retry_after_secs = retry;
        return Err(ApiError::rate_limited(format!(
            "relay task refused: {}",
            breach_reason(&statuses)
        ))
        .retry_after(retry)
        .proximity(prox));
    }
    Ok(statuses.iter().find(|s| s.warning).map(|s| {
        format!(
            "project '{}' is at {:.0}% of its {:?}/{:?} limit ({:.4} of {:.4}); relay runs count \
             toward it",
            s.project_id,
            s.ratio * 100.0,
            s.metric,
            s.window,
            s.current,
            s.threshold
        )
    }))
}

pub(crate) async fn enqueue_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnqueueReq>,
) -> Result<Json<EnqueueResp>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_ingest_project(&p, &req.project_id)?;
    // Trimmed ONCE, here, and the trimmed form is what is admitted, matched and stored. Admission
    // used to trim while the row kept the raw string, so `" xprice/summary"` passed the door as
    // routable and then sat queued forever: a device's capability match runs on the stored value.
    let action_type = req.action_type.trim().to_string();
    if action_type.is_empty() {
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
            // A replay is the SAME request again. The same key with a different action or payload
            // is a caller reusing keys across distinct work, and answering it with the old task
            // told them their new work was queued when nothing was. Stripe's rule, for Stripe's
            // reason: 409, naming the key.
            if existing.action_type != action_type || existing.payload != req.payload {
                return Err(ApiError::conflict(format!(
                    "idempotency_key {:?} was already used for a different request (action                      {:?}); pick a new key for new work",
                    req.idempotency_key.as_deref().unwrap_or_default(),
                    existing.action_type
                )));
            }
            let admission = admit(&st, &existing.action_type).await;
            // No budget check on this arm: the task already exists, so answering with it enqueues
            // nothing. Refusing a replay would break idempotency exactly when a caller is retrying.
            return Ok(Json(EnqueueResp {
                task: existing,
                admission,
                warning: None,
            }));
        }
    }
    // Admission (M18). Validation used to be "action_type is non-empty", so a typo was
    // indistinguishable from a healthy backlog: the task sat queued, was handed to devices that had
    // no such action, burned every attempt on "no action", and dead-lettered hours later. A 422
    // here costs the caller one round trip and names the fix.
    let admission = admit(&st, &action_type).await;
    if let RelayAdmission::Refused { reason } = &admission {
        return Err(ApiError::relay_unroutable(reason.clone()));
    }
    // Spend admission (M5), after routability: "nothing can run this" is a mistake in the request
    // and "you are over budget" is a fact about the project, and a caller with both should hear
    // about the typo it can fix.
    let warning = budget_allows(&st, &project).await?;
    let now = Utc::now();
    let task = RelayTask {
        id: new_id(),
        project_id: project,
        source: req.source,
        action_type,
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
        // Floored like `max_attempts`: a `0` interval made a deferred task (subscription window
        // exhausted) leasable again the instant it was handed back, so the device and the cloud
        // spun lease/defer at network speed for the rest of the window.
        retry_interval_secs: req
            .retry_interval_secs
            .unwrap_or(RELAY_DEFAULT_RETRY_INTERVAL_SECS)
            .max(1),
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
    Ok(Json(EnqueueResp {
        task,
        admission,
        warning,
    }))
}

pub(crate) async fn get_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<RelayTask>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    // The scope IS the authorization (M17): another project's task is not found, not refused.
    let sc = p.scope_owned();
    let task = spawn_db(move || store.get_relay_task(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("relay task '{id}' not found")))?;
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
    let tasks = spawn_db(move || {
        store.list_relay_tasks(project.as_deref().into(), status.as_deref(), limit)
    })
    .await?;
    Ok(Json(tasks))
}
