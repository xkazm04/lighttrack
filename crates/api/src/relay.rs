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
    new_id, LlmEvent, Operation, Provider, RelayOutcome, RelayTask, Status, TokenUsage,
    RELAY_DEFAULT_MAX_ATTEMPTS, RELAY_DEFAULT_RETRY_INTERVAL_SECS,
};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, bearer, ensure_can_admin, resolve_ingest_project, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// Device endpoints (lease / result) authenticate with the enrolled device key
/// (`LIGHTTRACK_RELAY_DEVICE_KEY`); an admin principal (or dev mode) also passes, for local testing.
async fn ensure_device(st: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if let (Some(expected), Some(token)) = (st.relay_device_key.as_ref(), bearer(headers)) {
        if &token == expected {
            return Ok(());
        }
    }
    ensure_can_admin(&authenticate(st, headers).await?)
}

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

pub(crate) async fn enqueue_task(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnqueueReq>,
) -> Result<Json<RelayTask>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_ingest_project(&p, &req.project_id)?;
    if req.action_type.trim().is_empty() {
        return Err(ApiError::bad_request("action_type is required"));
    }
    // Idempotent enqueue: the same (project, key) returns the existing task instead of a duplicate.
    if let Some(key) = req.idempotency_key.clone() {
        let store = st.store.clone();
        let project2 = project.clone();
        if let Some(existing) = spawn_db(move || store.find_relay_task_by_key(&project2, &key)).await? {
            return Ok(Json(existing));
        }
    }
    let now = Utc::now();
    let task = RelayTask {
        id: new_id(),
        project_id: project,
        source: req.source,
        action_type: req.action_type,
        payload: req.payload,
        status: "queued".to_string(),
        attempts: 0,
        max_attempts: req.max_attempts.unwrap_or(RELAY_DEFAULT_MAX_ATTEMPTS).max(1),
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
    Ok(Json(task))
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
    if let Principal::Project(pid) = &p {
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
    let store = st.store.clone();
    let status = q.status;
    let limit = q.limit.unwrap_or(50).min(1000);
    let tasks =
        spawn_db(move || store.list_relay_tasks(project.as_deref(), status.as_deref(), limit))
            .await?;
    Ok(Json(tasks))
}

#[derive(Deserialize)]
pub(crate) struct LeaseReq {
    #[serde(default = "default_device")]
    device: String,
    #[serde(default = "default_max")]
    max: usize,
    #[serde(default = "default_lease_secs")]
    lease_secs: i64,
    /// Long-poll: hold the request up to this many seconds until a task is due (0 = return
    /// immediately). Cuts pickup latency without shrinking the device's poll interval.
    #[serde(default)]
    wait_secs: u64,
}

fn default_device() -> String {
    "default".to_string()
}

fn default_max() -> usize {
    1
}

fn default_lease_secs() -> i64 {
    1800
}

pub(crate) async fn lease_tasks(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LeaseReq>,
) -> Result<Json<Vec<RelayTask>>, ApiError> {
    ensure_device(&st, &headers).await?;
    // Heavy Claude Code runs take a while — allow generous leases, but bound them so a crashed
    // device's tasks are reclaimable the same day.
    let lease_secs = req.lease_secs.clamp(60, 21_600);
    let max = req.max.clamp(1, 20);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(req.wait_secs.min(25));
    loop {
        // Sweep first so exhausted expired leases dead-letter (and alert) instead of lingering.
        let store = st.store.clone();
        let dead = spawn_db(move || store.sweep_relay_dead()).await?;
        if !dead.is_empty() {
            st.alerts.notify_relay_dead(&dead);
        }
        let store = st.store.clone();
        let device = req.device.clone();
        let tasks = spawn_db(move || store.lease_relay_tasks(&device, lease_secs, max)).await?;
        if !tasks.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(Json(tasks));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[derive(Deserialize)]
pub(crate) struct ResultReq {
    /// `succeeded` | `failed` | `deferred`.
    status: String,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
    /// For `deferred`: when to retry (defaults to the task's retry interval).
    #[serde(default)]
    retry_after_secs: Option<u32>,
    // Usage accounting from the CLI envelope, for the run's observability event.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    latency_ms: Option<u64>,
}

pub(crate) async fn post_result(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ResultReq>,
) -> Result<Json<RelayTask>, ApiError> {
    ensure_device(&st, &headers).await?;
    let outcome = match req.status.as_str() {
        "succeeded" => RelayOutcome::Succeeded(req.result.clone()),
        "failed" => RelayOutcome::Failed(
            req.error.clone().unwrap_or_else(|| "unspecified error".to_string()),
        ),
        "deferred" => RelayOutcome::Deferred {
            retry_after_secs: req.retry_after_secs,
            reason: req.error.clone(),
        },
        other => {
            return Err(ApiError::bad_request(format!(
                "status must be succeeded|failed|deferred, got '{other}'"
            )))
        }
    };
    // Whether this report actually lands (vs a duplicate of an already-settled task) decides
    // whether a usage event is logged; a task that is no longer leased settles as a no-op.
    let store = st.store.clone();
    let id2 = id.clone();
    let prior = spawn_db(move || store.get_relay_task(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("relay task '{id}' not found")))?;
    let store = st.store.clone();
    let id2 = id.clone();
    let task = spawn_db(move || store.settle_relay_task(&id2, &outcome))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("relay task '{id}' not found")))?;

    // A terminal-outcome report on a live lease consumed a real Claude run: record it at the
    // flat relay price (docs/RELAY.md). Always recorded — enforcing limits exist to cap metered
    // spend, and this run already happened on the flat-rate subscription. Deferred ⇒ no run.
    if prior.status == "leased" && req.status != "deferred" {
        let ev = relay_run_event(&st, &task, &req);
        let store = st.store.clone();
        spawn_db(move || store.insert_event(&ev)).await?;
        // A failure that exhausted the attempts just dead-lettered the task — page the owner.
        if task.status == "dead" {
            st.alerts.notify_relay_dead(std::slice::from_ref(&task));
        }
    }
    Ok(Json(task))
}

/// The observability event for one executed relay run. `trace_id` is the task id, so retried
/// attempts of the same task group into one trace.
fn relay_run_event(st: &AppState, task: &RelayTask, req: &ResultReq) -> LlmEvent {
    let failed = req.status == "failed";
    LlmEvent {
        id: new_id(),
        project_id: task.project_id.clone(),
        trace_id: Some(task.id.clone()),
        span_id: None,
        parent_span_id: None,
        ts: Utc::now(),
        provider: Provider::Anthropic,
        model: req.model.clone().unwrap_or_else(|| "claude-code".to_string()),
        operation: Operation::Chat,
        usage: TokenUsage {
            input: req.input_tokens.unwrap_or(0),
            output: req.output_tokens.unwrap_or(0),
            cached_input: None,
            reasoning: None,
        },
        cost_usd: Some(st.relay_flat_cost),
        latency_ms: req.latency_ms,
        status: if failed { Status::Error } else { Status::Success },
        error: if failed { req.error.clone() } else { None },
        input: None,
        output: None,
        tags: vec!["relay".to_string()],
        source: task.source.clone(),
        metadata: serde_json::json!({
            "task_id": task.id,
            "action_type": task.action_type,
            "attempt": task.attempts,
        }),
    }
}
