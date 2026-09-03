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
//!
//! A whole-trace verdict also records **what it judged** (`ScoreDetail::coverage`: the trace's span
//! count and a fingerprint of the judged root exchange). A trace has no completion signal, so a span
//! that lands after scoring is folded straight into the next read while the verdict stays put; the
//! detail read compares each verdict's coverage against the trace as it now stands and marks the
//! ones that stopped describing it (`scores[].stale`).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, LlmEvent, Score, ScoreDetail, Trace, TraceCoverage, TraceSpan};
use lighttrack_store::{TraceFilter, MAX_TRACE_SPANS};

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct TracesParams {
    project: Option<String>,
    limit: Option<usize>,
    /// RFC3339 bounds on the trace's `ended` time (`since` inclusive lower, `until` exclusive upper).
    since: Option<String>,
    until: Option<String>,
    /// `success` | `error` — keep only traces of that status.
    status: Option<String>,
    /// Minimum whole-trace cost (USD).
    min_cost: Option<f64>,
    /// Opaque keyset cursor from a prior page's `X-Next-Cursor` header.
    cursor: Option<String>,
}

/// Parse an optional RFC3339 query param into a UTC instant, 400 on malformed input.
fn parse_opt_ts(field: &str, raw: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    match raw {
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| ApiError::bad_request(format!("invalid '{field}' timestamp: {e}")))?
                .with_timezone(&Utc),
        )),
        None => Ok(None),
    }
}

/// List recent traces (one rollup row per `trace_id`), newest `ended` first. Optional `since`/`until`
/// window, `status`, and `min_cost` filters narrow the set; keyset paging on `(ended, trace_id)`
/// returns the next cursor in the `X-Next-Cursor` header when more traces remain (mirrors
/// `/v1/events`). The JSON body stays a bare array so render/CLI shape is unchanged.
pub(crate) async fn list_traces(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TracesParams>,
) -> Result<Response, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    if let Some(s) = q.status.as_deref() {
        if s != "success" && s != "error" {
            return Err(ApiError::bad_request("status must be 'success' or 'error'"));
        }
    }
    let since = parse_opt_ts("since", q.since.as_deref())?;
    let until = parse_opt_ts("until", q.until.as_deref())?;
    // The same refusals `/v1/events` makes: an inverted window and a non-finite cost floor both
    // used to page back empty and read as "no such traces".
    if let (Some(s), Some(u)) = (since, until) {
        if u <= s {
            return Err(ApiError::bad_request("until must be after since"));
        }
    }
    if q.min_cost.is_some_and(|c| !c.is_finite()) {
        return Err(ApiError::bad_request("min_cost must be a finite number"));
    }
    let filter = TraceFilter {
        since,
        until,
        status: q.status.clone(),
        min_cost: q.min_cost,
        cursor: q.cursor.clone(),
    };
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);
    let page =
        spawn_db(move || store.list_traces_filtered(project.as_deref().into(), &filter, limit))
            .await?;

    let mut resp = Json(page.traces).into_response();
    if let Some(cursor) = page.next_cursor {
        if let Ok(v) = HeaderValue::from_str(&cursor) {
            resp.headers_mut().insert("x-next-cursor", v);
        }
    }
    Ok(resp)
}

/// The detail payload: the trace rollup flattened together with the scores recorded within it.
#[derive(Serialize)]
pub(crate) struct TraceDetail {
    #[serde(flatten)]
    trace: Trace,
    scores: Vec<TraceScoreView>,
}

/// A score as it reads *against this trace right now*: the stored row, plus whether the verdict has
/// stopped covering the trace it was written about. Flattened, so every existing consumer of
/// `scores[]` sees the same fields it always did.
#[derive(Serialize)]
pub(crate) struct TraceScoreView {
    #[serde(flatten)]
    score: Score,
    /// Present only when the verdict no longer covers the trace as it now reads. Absent for a
    /// current verdict *and* for one that recorded no coverage (an older or third-party score) —
    /// silence means "nothing to report", never "verified fresh".
    #[serde(skip_serializing_if = "Option::is_none")]
    stale: Option<VerdictStaleness>,
}

/// Why a stored verdict no longer covers its trace, in the terms an operator needs to act on.
#[derive(Serialize)]
pub(crate) struct VerdictStaleness {
    /// `changed` — the judged root exchange itself differs, so the verdict describes text that is no
    /// longer there (this is what earns a re-score). `grown` — the same exchange, but the trace has
    /// gained or lost spans since; the verdict is narrower than the trace it sits on.
    reason: &'static str,
    scored_spans: usize,
    current_spans: usize,
}

/// Compare each stored verdict's recorded coverage against the trace as it now reads.
fn views(scores: Vec<Score>, current: &TraceCoverage) -> Vec<TraceScoreView> {
    scores
        .into_iter()
        .map(|score| {
            let stale = score
                .detail
                .as_ref()
                .and_then(|d| d.coverage.as_ref())
                .and_then(|cov| {
                    cov.drift(current).reason().map(|reason| VerdictStaleness {
                        reason,
                        scored_spans: cov.spans,
                        current_spans: current.spans,
                    })
                });
            TraceScoreView { score, stale }
        })
        .collect()
}

/// One trace: totals + span tree, plus any per-call or whole-trace scores attached to it.
pub(crate) async fn get_trace(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<TraceDetail>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let scope = resolve_read_project(&p, None)?;
    let trace = load_trace(&st, scope.clone(), &id).await?;

    let store = st.store.clone();
    let tid = id.clone();
    let scores = spawn_db(move || store.list_trace_scores(scope.as_deref().into(), &tid)).await?;
    let scores = views(scores, &trace.coverage());
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
    // Reads the trace, then writes a verdict about it — so both capabilities, and the same
    // `Ingest` the sibling `POST /v1/scores` door requires (a verdict is a recorded observation,
    // not a configuration change).
    crate::auth_scopes::ensure_scope(&p, lighttrack_core::Scope::Ingest)?;
    let scope = resolve_read_project(&p, None)?;
    let trace = load_trace(&st, scope, &id).await?;

    // Every span, not only the roots: `trace.spans` is a forest whose inner calls sit in
    // `children`. Walking the top level alone meant a per-call verdict on a nested span never
    // found its event, and a whole-trace verdict's evidence count covered the root spans only.
    let events = all_events(&trace.spans);
    if let Some(anchor) = body.event_id.as_deref() {
        // The anchor must be one of this trace's own spans: an id from elsewhere (another trace,
        // another project) used to be stored as the verdict's event_id unchecked.
        if !events.iter().any(|e| e.id == anchor) {
            return Err(ApiError::bad_request(format!(
                "event_id {anchor:?} is not a span of trace '{id}'"
            )));
        }
    }
    // Anchor to the requested call, else the trace's entry-point span.
    let event_id = body
        .event_id
        .or_else(|| trace.root_event_id().map(str::to_string));
    // A verdict anchored to the root is a judgment of the *whole trace*, so it records what it
    // judged: the trace has no end marker, and without this receipt a span landing a second later
    // silently widens the trace while the verdict stays put. A verdict pinned to a specific inner
    // call is a per-call score — whole-trace coverage would misdescribe it, so it gets none.
    // How mangled the judged evidence was. The judge reads the *stored* text, so a scrub that
    // rewrote a payload changed what was judged — a per-call verdict reports its own event's spans,
    // a whole-trace verdict the sum across the spans it covers. `None` (rather than 0) when no
    // covered event carried a stamp: "we do not know" is a weaker claim than "nothing was rewritten"
    // and must not be dressed up as it.
    let judged: Vec<&LlmEvent> = match event_id.as_deref() {
        Some(anchor) if Some(anchor) != trace.root_event_id() => {
            events.iter().copied().filter(|e| e.id == anchor).collect()
        }
        _ => events.clone(),
    };
    let evidence_redacted_spans = judged
        .iter()
        .filter_map(|e| e.redaction())
        .map(|r| r.spans)
        .reduce(|a, b| a.saturating_add(b));
    let whole_trace = event_id.is_some() && event_id.as_deref() == trace.root_event_id();
    let detail = (whole_trace || evidence_redacted_spans.is_some()).then(|| ScoreDetail {
        coverage: whole_trace.then(|| trace.coverage()),
        evidence_redacted_spans,
        ..Default::default()
    });
    let score = Score {
        id: new_id(),
        project_id: trace.project_id.clone(),
        event_id,
        rubric: body.rubric,
        // A verdict on a whole trace is a trace verdict; one pinned to an inner call is a
        // per-call score against whatever rubric the caller names, which is freeform here.
        rubric_id: None,
        kind: if whole_trace {
            lighttrack_core::ScoreKind::Trace
        } else {
            lighttrack_core::ScoreKind::Freeform
        },
        value: body.value,
        max: body.max,
        pass: body.pass,
        reasoning: body.reasoning,
        detail,
        // A trace score is an ad-hoc human/API verdict, not a benchmark case.
        run_id: None,
        case_index: None,
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

/// Every event in a span forest, depth-first — roots and their nested calls alike.
fn all_events(spans: &[TraceSpan]) -> Vec<&LlmEvent> {
    fn walk<'a>(spans: &'a [TraceSpan], out: &mut Vec<&'a LlmEvent>) {
        for s in spans {
            out.push(&s.event);
            walk(&s.children, out);
        }
    }
    let mut out = Vec::new();
    walk(spans, &mut out);
    out
}

/// Fetch a trace by id **within `scope`**, mapping an unknown trace to 404.
///
/// A `trace_id` is caller-supplied, so isolation is enforced by the query's project filter rather
/// than by authorizing a cross-project merge after the fact: two projects using the same natural id
/// (accidentally or otherwise) each see only their own spans. Consequently another project's trace
/// reads as 404 (not 403) — it is invisible, which also removes the existence oracle. `scope` is
/// `None` only for admin/dev, whose deliberate cross-project view is preserved.
async fn load_trace(st: &AppState, scope: Option<String>, id: &str) -> Result<Trace, ApiError> {
    let store = st.store.clone();
    let tid = id.to_string();
    spawn_db(move || store.get_trace(scope.as_deref().into(), &tid, MAX_TRACE_SPANS))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("trace '{id}' not found")))
}
