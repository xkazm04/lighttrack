//! Event ingest + querying, and cost summaries.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use lighttrack_core::{LimitStatus, LlmEvent, Status};
use lighttrack_store::{Admission, CostRow, EventFilter, UseCaseCostRow};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::events_validate::policy;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// Scope one event to its project, validate it, enforce the project's payload-persistence policy,
/// scrub PII, and fill/mark its cost — everything the single- and batch-ingest paths share up to the
/// admission step. On a validation failure returns the human-facing 400 message (the batch path
/// records it per item; the single path maps it to a 400).
pub(crate) fn prepare_event(
    st: &AppState,
    ev: &mut LlmEvent,
    pid: &str,
    persistence: lighttrack_core::Redaction,
) -> Result<(), String> {
    ev.project_id = pid.to_string();
    policy().validate(ev, Utc::now())?;
    // 1. The project's stored persistence policy (hash/drop) — the setting the projects API accepts
    // and the operator table displays, now actually enforced. Applied before the PII scrub: `drop`
    // removes the payloads outright; `hash` leaves nothing scrubbable.
    if crate::redact::apply_policy(ev, persistence) {
        eprintln!("[REDACT] project={pid} event={} applied persistence policy {persistence:?}", ev.id);
    }
    // 2. Optional env-configured floor: scrub structured PII from what remains before it is stored.
    let redacted = st.redact.redact_event(ev);
    if redacted > 0 {
        eprintln!("[REDACT] project={pid} event={} redacted {redacted} PII span(s)", ev.id);
    }
    let client_supplied = ev.cost_usd.is_some();
    {
        let book = st.prices.read().unwrap();
        ev.ensure_cost(&book);
    }
    mark_cost_source(ev, client_supplied);
    Ok(())
}

/// Record how an event's `cost_usd` was determined so downstream margin/forecast can trust or discount
/// it: `"client"` when the caller supplied a cost verbatim, `"book"` when we priced it from the DB
/// price book. Stamped into `metadata` (not a column) so every store backend carries it unchanged.
fn mark_cost_source(ev: &mut LlmEvent, client_supplied: bool) {
    if ev.cost_usd.is_none() {
        return; // no cost resolved (unpriced) → nothing to attribute
    }
    let src = Value::String(if client_supplied { "client" } else { "book" }.to_string());
    match &mut ev.metadata {
        Value::Object(m) => {
            m.insert("cost_source".to_string(), src);
        }
        v @ Value::Null => *v = Value::Object([("cost_source".to_string(), src)].into_iter().collect()),
        _ => {} // non-object, non-null metadata is client-owned: don't clobber it
    }
}

/// Post-admission side effects shared by the single- and batch-ingest paths: log and best-effort
/// deliver breach alerts, count a rejected event into the rejection ledger, and (for an admitted
/// non-success call) feed error-spike detection. Returns the breached statuses so the caller can
/// shape its response (429 vs. observe-only flag).
pub(crate) fn on_admission(st: &AppState, ev: &LlmEvent, admission: &Admission) -> Vec<LimitStatus> {
    let breached: Vec<LimitStatus> =
        admission.statuses.iter().filter(|s| s.breached).cloned().collect();
    for b in &breached {
        eprintln!(
            "[ALERT] project={} metric={:?} window={:?} value={:.6} >= threshold={:.6} action={:?}",
            b.project_id, b.metric, b.window, b.current, b.threshold, b.action
        );
    }
    // A rejected event is never stored (that would corrupt usage/cost), so count it out-of-band in the
    // best-effort rejection ledger — the running per-key count then rides along on the breach alert.
    // Its estimated cost is the priced `cost_usd` if we resolved one, else $0 (unpriced).
    let rej_counts = if admission.admitted {
        std::collections::HashMap::new()
    } else {
        record_rejection(st, ev, &breached)
    };
    // Best-effort, off the request path: deliver breaches to webhook/ntfy (deduped per cooldown).
    st.alerts.notify(&breached, &rej_counts);
    // Soft-warning tier: for an *admitted* event, alert on any rule that crossed its warn_at without
    // breaching — the operator's early heads-up before the cap actually bites. Only when admitted, so
    // the usage the warning reports genuinely includes a recorded event (a rejected event isn't stored).
    if admission.admitted {
        let warnings: Vec<LimitStatus> =
            admission.statuses.iter().filter(|s| s.warning).cloned().collect();
        if !warnings.is_empty() {
            st.alerts.notify_warnings(&warnings);
        }
    }
    // Best-effort error-spike detection: only admitted non-success calls count toward the threshold.
    if admission.admitted && ev.status != Status::Success {
        st.alerts.record_error(ev);
    }
    breached
}

/// Fold a just-rejected event into the rejection ledger — once per enforcing breach that turned it
/// away — and return the running rejection count for each, keyed the same way the alerter dedups
/// breaches ([`LimitStatus::alert_key`], which includes the scope) so the count can be attached to
/// the outgoing alert.
fn record_rejection(
    st: &AppState,
    ev: &LlmEvent,
    breached: &[LimitStatus],
) -> std::collections::HashMap<String, u64> {
    let cost = ev.cost_usd.unwrap_or(0.0);
    let now = Utc::now();
    let mut counts = std::collections::HashMap::new();
    for b in breached.iter().filter(|s| s.rejects_ingest()) {
        let count =
            st.rejections.record(&b.project_id, b.metric, b.window, b.scope.clone(), cost, now);
        counts.insert(b.alert_key(), count);
    }
    counts
}

/// Human-facing reason an admission was rejected (the enforcing breach that caused the 429).
pub(crate) fn breach_reason(breached: &[LimitStatus]) -> String {
    breached
        .iter()
        .find(|s| s.action.enforces())
        .map(|s| {
            let scope = match &s.scope {
                Some(sc) => format!(" [scope {}]", sc.label()),
                None => String::new(),
            };
            format!(
                "ingest blocked: project '{}'{scope} is over its {:?}/{:?} limit \
                 ({:.4} >= {:.4}, action={:?})",
                s.project_id, s.metric, s.window, s.current, s.threshold, s.action
            )
        })
        .unwrap_or_else(|| "ingest blocked: usage limit exceeded".to_string())
}

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
    let persistence = crate::state::redaction_policy_for(&st, &pid).await?;
    prepare_event(&st, &mut ev, &pid, persistence).map_err(ApiError::bad_request)?;

    // Admission control: evaluate the project's limits and insert in one atomic store step. An
    // enforcing (Throttle/Block) breach rejects the event — it is NOT recorded and we return 429 so
    // a cooperating client backs off. This is what makes a configured cap an actual cap, not a flag.
    let store = st.store.clone();
    let to_insert = ev.clone();
    let admission = spawn_db(move || store.insert_event_checked(&to_insert)).await?;

    let breached = on_admission(&st, &ev, &admission);
    if !admission.admitted {
        return Err(ApiError::rate_limited(breach_reason(&breached)));
    }

    // Admitted: any remaining breaches are Alert-only (enforcing ones would have 429'd above).
    let throttled = breached.iter().any(|s| s.rejects_ingest());
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
    /// RFC3339 lower/upper bounds on event time (`since` inclusive, `until` exclusive).
    since: Option<String>,
    until: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    trace_id: Option<String>,
    name: Option<String>,
    /// Opaque keyset cursor from a prior page's `X-Next-Cursor` header.
    cursor: Option<String>,
    /// When `1`/`true`, return only the most recent events that do not yet have a score (the online
    /// scorer's work list). Uses a scoped anti-join, so it stays correct however large `scores` grows;
    /// ignores the filter/cursor params (project + limit only).
    unscored: Option<bool>,
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

/// Paged, filtered event listing. The JSON body stays a bare array (render/CLI shape unchanged); when
/// more rows remain the next keyset cursor is returned in the `X-Next-Cursor` response header.
pub(crate) async fn get_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsParams>,
) -> Result<Response, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let filter = EventFilter {
        since: parse_opt_ts("since", q.since.as_deref())?,
        until: parse_opt_ts("until", q.until.as_deref())?,
        provider: q.provider.clone(),
        model: q.model.clone(),
        trace_id: q.trace_id.clone(),
        name: q.name.clone(),
        cursor: q.cursor.clone(),
    };
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);

    // Unscored work-list mode: a scoped anti-join (project + limit only), no filter/cursor. Bare
    // array, no next-cursor — the online scorer pages by re-asking after it has scored a batch.
    if q.unscored == Some(true) {
        let events =
            spawn_db(move || store.list_unscored_events(project.as_deref(), limit)).await?;
        return Ok(Json(events).into_response());
    }

    let page =
        spawn_db(move || store.list_events_filtered(project.as_deref(), &filter, limit)).await?;

    let mut resp = Json(page.events).into_response();
    if let Some(cursor) = page.next_cursor {
        if let Ok(v) = HeaderValue::from_str(&cursor) {
            resp.headers_mut().insert("x-next-cursor", v);
        }
    }
    Ok(resp)
}

#[derive(Deserialize)]
pub(crate) struct CostsParams {
    project: Option<String>,
    /// Optional RFC3339 window (`since` inclusive, `until` exclusive); omit for full history.
    since: Option<String>,
    until: Option<String>,
}

pub(crate) async fn get_costs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CostsParams>,
) -> Result<Json<Vec<CostRow>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let since = parse_opt_ts("since", q.since.as_deref())?;
    let until = parse_opt_ts("until", q.until.as_deref())?;
    let store = st.store.clone();
    let rows =
        spawn_db(move || store.cost_summary_windowed(project.as_deref(), since, until)).await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub(crate) struct UsecasesParams {
    project: Option<String>,
    /// RFC3339 lower bound (inclusive) on event time — the rolling-window start.
    since: Option<String>,
}

/// Use-case rollup: usage + cost grouped by (name, provider, model), optionally windowed by `since`.
/// Powers the Personas "LLM Overview" table; a call's use-case is its `name`, or its model when
/// unnamed. Read-scoped like the other list endpoints (a project key sees only its own project).
pub(crate) async fn get_usecases(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UsecasesParams>,
) -> Result<Json<Vec<UseCaseCostRow>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let since = match q.since.as_deref() {
        Some(s) => Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| ApiError::bad_request(format!("invalid 'since' timestamp: {e}")))?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    let store = st.store.clone();
    let rows = spawn_db(move || store.usecase_costs(project.as_deref(), since)).await?;
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
