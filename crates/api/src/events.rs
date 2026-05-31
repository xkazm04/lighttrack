//! Event ingest + querying, and cost summaries.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{LimitStatus, LlmEvent};
use lighttrack_store::CostRow;

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use crate::limits::{evaluate_project_limits, is_throttle};
use crate::state::{spawn_db, AppState};

#[derive(Serialize)]
pub(crate) struct IngestResponse {
    id: String,
    project_id: String,
    cost_usd: Option<f64>,
    ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    breached: Vec<LimitStatus>,
    throttled: bool,
}

pub(crate) async fn post_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut ev): Json<LlmEvent>,
) -> Result<Json<IngestResponse>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    let pid = resolve_ingest_project(&principal, &ev.project_id)?;
    ev.project_id = pid.clone();
    // Optional: scrub structured PII from captured input/output before it is stored.
    let redacted = st.redact.redact_event(&mut ev);
    if redacted > 0 {
        eprintln!("[REDACT] project={pid} event={} redacted {redacted} PII span(s)", ev.id);
    }
    {
        let book = st.prices.read().unwrap();
        ev.ensure_cost(&book);
    }

    let store = st.store.clone();
    let to_insert = ev.clone();
    spawn_db(move || store.insert_event(&to_insert)).await?;

    let statuses = evaluate_project_limits(&st, &pid).await?;
    let breached: Vec<LimitStatus> = statuses.into_iter().filter(|s| s.breached).collect();
    let throttled = breached.iter().any(is_throttle);
    for b in &breached {
        eprintln!(
            "[ALERT] project={} metric={:?} window={:?} value={:.6} >= threshold={:.6} action={:?}",
            b.project_id, b.metric, b.window, b.current, b.threshold, b.action
        );
    }
    // Best-effort, off the request path: deliver breaches to webhook/ntfy (deduped per cooldown).
    st.alerts.notify(&breached);

    Ok(Json(IngestResponse {
        id: ev.id,
        project_id: pid,
        cost_usd: ev.cost_usd,
        ts: ev.ts,
        breached,
        throttled,
    }))
}

#[derive(Deserialize)]
pub(crate) struct EventsParams {
    project: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn get_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsParams>,
) -> Result<Json<Vec<LlmEvent>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);
    let events = spawn_db(move || store.list_events(project.as_deref(), limit)).await?;
    Ok(Json(events))
}

#[derive(Deserialize)]
pub(crate) struct ProjectParam {
    project: Option<String>,
}

pub(crate) async fn get_costs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ProjectParam>,
) -> Result<Json<Vec<CostRow>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let rows = spawn_db(move || store.cost_summary(project.as_deref())).await?;
    Ok(Json(rows))
}

pub(crate) async fn get_event_by_id(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<LlmEvent>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let ev = spawn_db(move || store.get_event(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("event '{id}' not found")))?;
    if let Principal::Project(pid) = &p {
        if &ev.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that event's project"));
        }
    }
    Ok(Json(ev))
}
