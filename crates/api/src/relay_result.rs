//! The device's result report: the one relay door that both settles a task and writes an event.
//!
//! Split from [`crate::relay`] because it carries the queue's only ingest path — a server-generated
//! observability event for the run that just happened — and that has its own rules (flat price,
//! explicit PII scrub, disabled projects settle but record nothing).

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::{
    new_id, LlmEvent, Operation, RelayOutcome, RelaySettle, RelayStatus, RelayTask, Status,
    TokenUsage,
};

use crate::error::ApiError;
use crate::relay_devices::ensure_device;
use crate::state::{spawn_db, AppState};

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
    /// What the device's CLI envelope said the run cost. Recorded as **evidence**, in the event's
    /// metadata — the relay's `cost_usd` stays the flat price (docs/RELAY.md, D5): switching to
    /// token/envelope pricing is a separate decision, and silently doing it here would move every
    /// margin number without anyone asking.
    #[serde(default)]
    cost_usd: Option<f64>,
    /// The posture the run executed under (`generate` | `readonly-scan` | `edit`). The cloud names
    /// only an `action_type`, so without this the record cannot say whether a repository was read
    /// or written.
    #[serde(default)]
    mode: Option<String>,
    /// sha256 of the **rendered** prompt this run executed — the text the model actually read,
    /// params substituted. The relay is the one LLM workload LightTrack originates, and its action
    /// prompts live on the device with no version and no registry: without this the cloud cannot
    /// tell a prompt that regressed six weeks ago from one that never changed.
    #[serde(default)]
    prompt_sha256: Option<String>,
    /// The `version` the action declares, if any — the fingerprint's human-readable companion.
    #[serde(default)]
    action_version: Option<String>,
    /// The rendered prompt and the result text, present only when the action set `report_io`
    /// (`actions/README.md`). They become the event's `input`/`output`, which is what makes a relay
    /// run judgeable at all — both judges skip an event with no content. Absent by default: an
    /// action's prompt and result are the two things `docs/RELAY.md` promises stay on the device
    /// unless the operator says otherwise, per action.
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<Value>,
    /// The `lease_fence` this device was handed at lease time. Omitting it is the operator-shaped
    /// settle, which waives the ownership condition but never the liveness one — a device always
    /// sends it, and that is what makes a reclaimed device's late report a refusal rather than a
    /// silent overwrite of its successor's run.
    #[serde(default)]
    fence: Option<chrono::DateTime<Utc>>,
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
            req.error
                .clone()
                .unwrap_or_else(|| "unspecified error".to_string()),
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
    // Whether this report actually lands decides whether a usage event is logged. A refusal is a
    // 409, not a silent no-op: the device has to learn it lost the task so it stops and does not
    // deliver, and the run it did perform is NOT recorded against a task somebody else now owns.
    let store = st.store.clone();
    let id2 = id.clone();
    let fence = req.fence;
    let settled = spawn_db(move || store.settle_relay_task(&id2, fence, &outcome)).await?;
    let task = match settled {
        RelaySettle::Settled(t) => *t,
        RelaySettle::NoSuchTask => {
            return Err(ApiError::not_found(format!("relay task '{id}' not found")))
        }
        RelaySettle::NotHeld { status, fence } => {
            return Err(ApiError::conflict(format!(
                "relay task '{id}' is {status} and not held by that lease (its lease is \
                 {fence:?}); the result you sent was NOT recorded"
            )))
        }
    };

    // The report landed on a live lease, so it consumed a real Claude run: record it at the flat
    // relay price (docs/RELAY.md). Always recorded — enforcing limits exist to cap metered spend,
    // and this run already happened on the flat-rate subscription. Deferred ⇒ no run.
    if req.status != "deferred" {
        // The fourth ingest door, and the one that cannot answer 403: the device has already run
        // the work, and refusing the report would leave the task leased until it expired and was
        // retried forever. So a disabled project settles the task (it terminates) and records
        // nothing — `enabled` is honoured where it matters, on what lands in the store.
        let policy = crate::state::project_policy_for(&st, &task.project_id).await?;
        if !policy.enabled {
            tracing::info!(
                project_id = %task.project_id,
                task_id = %task.id,
                "relay run settled but not recorded: the project is disabled"
            );
            return Ok(Json(task));
        }
        let mut ev = relay_run_event(&st, &task, &req);
        // This is the one door that writes an event without going through `events::prepare_event`,
        // and that is deliberate: the event is server-generated, so it needs no validation, costing
        // or limit admission (see below). What it does carry is `error` — device-supplied free text
        // that routinely echoes the task payload it failed on — so the PII scrub every other door
        // applies has to be applied here explicitly, or `docs/RELAY.md`'s claim that "ingest
        // redaction applies" is false exactly where a failure dumps the payload into the DB.
        let redacted = st
            .redact
            .redact_event(&mut ev, lighttrack_core::Redaction::None);
        if redacted > 0 {
            tracing::debug!(
                project_id = %ev.project_id,
                event_id = %ev.id,
                task_id = %task.id,
                spans = redacted,
                "scrubbed PII from a relay run event",
            );
        }
        let store = st.store.clone();
        spawn_db(move || store.insert_event(&ev)).await?;
        // A failure that exhausted the attempts just dead-lettered the task — page the owner.
        if task.status == RelayStatus::Dead.as_str() {
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
        received_at: Utc::now(),
        provider: "anthropic".into(),
        model: req
            .model
            .clone()
            .unwrap_or_else(|| "claude-code".to_string()),
        name: Some("relay-run".to_string()),
        operation: Operation::Chat,
        usage: TokenUsage {
            input: req.input_tokens.unwrap_or(0),
            output: req.output_tokens.unwrap_or(0),
            cached_input: None,
            reasoning: None,
        },
        cost_usd: Some(st.relay_flat_cost),
        latency_ms: req.latency_ms,
        status: if failed {
            Status::Error
        } else {
            Status::Success
        },
        error: if failed { req.error.clone() } else { None },
        // Content when the action opted in, `None` otherwise — and `None` is what both judges read
        // as "skip". That is the whole gate: the relay run is judgeable exactly when its action
        // says its prompt and result may be stored here.
        input: req.input.clone(),
        output: req.output.clone(),
        tags: vec!["relay".to_string()],
        source: task.source.clone(),
        metadata: serde_json::json!({
            "task_id": task.id,
            // Also the key `GET /v1/relay/actions` groups by. It has always been on the row; what
            // is new is that `prompt_sha256` sits beside it, so "which prompt" is answerable and
            // not only "which action".
            "action_type": task.action_type,
            "attempt": task.attempts,
            // Reported by the device, not billed here — see `ResultReq::cost_usd`.
            "device_cost_usd": req.cost_usd,
            "mode": req.mode,
            "prompt_sha256": req.prompt_sha256,
            "action_version": req.action_version,
        }),
    }
}
