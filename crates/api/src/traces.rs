//! Traces: roll the events of one user request (sharing a `trace_id`) into an end-to-end view, and
//! score a whole trace rather than a single call.
//!
//! - `GET  /v1/traces?project=&limit=`  list recent traces (compact rollups, newest first)
//! - `GET  /v1/traces/:id`              one trace: totals + span tree + any scores within it
//! - `POST /v1/traces/:id/score`        record a judge verdict for the whole trace
//!
//! Trace scoring reuses the `scores` table: the verdict is anchored to the trace's root span event
//! (unless the body names a specific `event_id`), so it links back to the trace through the same
//! `event_id → trace_id` path the read side joins on — no separate schema.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, Score, Trace, TraceSummary};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct TracesParams {
    project: Option<String>,
    limit: Option<usize>,
}

/// List recent traces (one rollup row per `trace_id`), newest activity first.
pub(crate) async fn list_traces(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TracesParams>,
) -> Result<Json<Vec<TraceSummary>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);
    let traces = spawn_db(move || store.list_traces(project.as_deref(), limit)).await?;
    Ok(Json(traces))
}

/// The detail payload: the trace rollup flattened together with the scores recorded within it.
#[derive(Serialize)]
pub(crate) struct TraceDetail {
    #[serde(flatten)]
    trace: Trace,
    scores: Vec<Score>,
}

/// One trace: totals + span tree, plus any per-call or whole-trace scores attached to it.
pub(crate) async fn get_trace(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<TraceDetail>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let trace = load_trace(&st, &id).await?;
    authorize_trace(&p, &trace.project_id)?;

    let store = st.store.clone();
    let tid = id.clone();
    let scores = spawn_db(move || store.list_trace_scores(&tid)).await?;
    Ok(Json(TraceDetail { trace, scores }))
}

/// Body for scoring a whole trace — a judge verdict without the trace/project plumbing the caller
/// shouldn't have to repeat. `event_id` is optional: omit it to anchor the score to the trace's root
/// span (the whole-request judgment), or set it to attach the verdict to a specific call.
#[derive(Deserialize)]
pub(crate) struct TraceScoreBody {
    rubric: String,
    value: f64,
    #[serde(default = "one")]
    max: f64,
    #[serde(default)]
    pass: Option<bool>,
    #[serde(default)]
    reasoning: Option<String>,
    scored_by: String,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    event_id: Option<String>,
}

fn one() -> f64 {
    1.0
}

/// Record a score for a whole trace. The verdict anchors to the named `event_id`, or the trace's
/// root span when none is given.
pub(crate) async fn score_trace(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TraceScoreBody>,
) -> Result<Json<Score>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let trace = load_trace(&st, &id).await?;
    authorize_trace(&p, &trace.project_id)?;

    // Anchor to the requested call, else the trace's entry-point span.
    let event_id = body.event_id.or_else(|| trace.root_event_id().map(str::to_string));
    let score = Score {
        id: new_id(),
        project_id: trace.project_id.clone(),
        event_id,
        rubric: body.rubric,
        value: body.value,
        max: body.max,
        pass: body.pass,
        reasoning: body.reasoning,
        scored_by: body.scored_by,
        cost_usd: body.cost_usd,
        created_at: Utc::now(),
    };

    let store = st.store.clone();
    let to_insert = score.clone();
    spawn_db(move || store.insert_score(&to_insert)).await?;
    st.alerts.record_score(&score);
    Ok(Json(score))
}

/// Fetch a trace by id, mapping an unknown trace to 404.
async fn load_trace(st: &AppState, id: &str) -> Result<Trace, ApiError> {
    let store = st.store.clone();
    let tid = id.to_string();
    spawn_db(move || store.get_trace(&tid))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("trace '{id}' not found")))
}

/// A project key may only touch traces in its own project; admin/dev may touch any.
fn authorize_trace(p: &Principal, project_id: &str) -> Result<(), ApiError> {
    if let Principal::Project(pid) = p {
        if pid != project_id {
            return Err(ApiError::forbidden("key not authorized for that trace's project"));
        }
    }
    Ok(())
}
