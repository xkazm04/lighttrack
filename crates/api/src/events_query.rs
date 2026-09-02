//! Event reads: the filtered/paged listing, cost rollups (`/v1/costs`, `/v1/costs/prompts`), the
//! use-case rollup, and fetch by id. Every read is project-scoped through `resolve_read_project`.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use lighttrack_core::LlmEvent;
use lighttrack_store::{CostRow, EventFilter, UseCaseCostRow};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

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
    /// Call outcome: `success` | `error` | `timeout`.
    status: Option<String>,
    /// Match events carrying this tag (array membership, not substring).
    tag: Option<String>,
    /// A metadata predicate: `key` (the key is present) or `key=value` (it equals `value`). This is
    /// how per-customer / per-product questions are asked, since that linkage rides in `metadata`
    /// rather than a column. Split on the FIRST `=`, so values may contain `=`.
    meta: Option<String>,
    /// Minimum resolved `cost_usd`, inclusive.
    min_cost: Option<f64>,
    /// Match rows the ingest scrub stamped with this rule-set fingerprint
    /// (`metadata.redaction.rules`; see `GET /v1/projects/:id/redaction` for the fingerprints
    /// present). The query that separates a cohort scrubbed by the current rules from one scrubbed
    /// by a previous generation of them.
    redaction_rules: Option<String>,
    /// Minimum spans the scrub replaced, inclusive. `1` is "everything the scrubber rewrote" — the
    /// candidate set for "did we mangle the evidence a judge read".
    min_redacted_spans: Option<u32>,
    /// When `1`/`true`, also return the total number of matching events in `X-Total-Count`. Opt-in:
    /// it costs a second aggregate query. Taken as a string because a query string carries `1`/`0`
    /// as often as `true`/`false`, and a strict bool parse would 400 on the common form.
    count: Option<String>,
    /// Opaque keyset cursor from a prior page's `X-Next-Cursor` header.
    cursor: Option<String>,
    /// When `1`/`true`, return only the most recent events that do not yet have a score (the online
    /// scorer's work list). Uses a scoped anti-join, so it stays correct however large `scores` grows;
    /// ignores the filter/cursor params (project + limit only). A string for the same reason as
    /// `count` above — this was typed `bool` while the docs promised `1`, so the runner's
    /// `?unscored=1` 400'd and online scoring could not make a single judge call.
    unscored: Option<String>,
}

/// Whether a query-string flag reads as set: `1` / `true` / `yes`, case-insensitive (the doc said
/// case-insensitive while the match arms only spelled out `TRUE`, so `True`/`Yes` were rejected).
///
/// Anything else — absent, `0`, `false`, or a typo — reads as *unset* rather than being a 400. These
/// are opt-in flags: the honest answer to "I could not parse your flag" is the behaviour you get
/// without the flag, and a hard reject would re-create the failure this helper exists to avoid.
fn is_truthy(v: Option<&str>) -> bool {
    v.map(str::trim).is_some_and(|s| {
        s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
    })
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
    let (metadata_key, metadata_value) = match q.meta.as_deref() {
        None => (None, None),
        Some(m) => match m.split_once('=') {
            Some((k, v)) if !k.is_empty() => (Some(k.to_string()), Some(v.to_string())),
            _ if !m.is_empty() => (Some(m.to_string()), None),
            _ => return Err(ApiError::bad_request("'meta' must be `key` or `key=value`")),
        },
    };
    if let Some(s) = q.status.as_deref() {
        // Reject an unknown outcome rather than answering with an empty page: "no errored calls"
        // and "you spelled the status wrong" must not look identical.
        // The accepted set is derived from the `Status` enum (the vocabulary's one authority) rather
        // than a hand-maintained literal list, so adding a `Status` variant cannot leave this filter
        // silently rejecting a valid new outcome.
        if lighttrack_core::Status::from_wire(s).is_none() {
            let expected = lighttrack_core::Status::ALL
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            return Err(ApiError::bad_request(format!(
                "invalid 'status' {s:?}: expected {expected}"
            )));
        }
    }
    let filter = EventFilter {
        since: parse_opt_ts("since", q.since.as_deref())?,
        until: parse_opt_ts("until", q.until.as_deref())?,
        provider: q.provider.clone(),
        model: q.model.clone(),
        trace_id: q.trace_id.clone(),
        name: q.name.clone(),
        status: q.status.clone(),
        tag: q.tag.clone(),
        metadata_key,
        metadata_value,
        min_cost: q.min_cost,
        redaction_rules: q.redaction_rules.clone(),
        min_redacted_spans: q.min_redacted_spans,
        with_total: is_truthy(q.count.as_deref()),
        cursor: q.cursor.clone(),
    };
    let store = st.store.clone();
    let limit = q.limit.unwrap_or(50).min(1000);

    // Unscored work-list mode: a scoped anti-join (project + limit only), no filter/cursor. Bare
    // array, no next-cursor — the online scorer pages by re-asking after it has scored a batch.
    if is_truthy(q.unscored.as_deref()) {
        let events =
            spawn_db(move || store.list_unscored_events(project.as_deref(), limit)).await?;
        return Ok(Json(events).into_response());
    }

    let page =
        spawn_db(move || store.list_events_filtered(project.as_deref(), &filter, limit)).await?;

    // The body stays a bare array (the render/CLI shape is a contract), so pagination metadata rides
    // in headers: the next keyset cursor, and — when asked for — the size of the whole matching set.
    let total = page.total;
    let mut resp = Json(page.events).into_response();
    if let Some(cursor) = page.next_cursor {
        if let Ok(v) = HeaderValue::from_str(&cursor) {
            resp.headers_mut().insert("x-next-cursor", v);
        }
    }
    if let Some(n) = total {
        if let Ok(v) = HeaderValue::from_str(&n.to_string()) {
            resp.headers_mut().insert("x-total-count", v);
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

/// Cost grouped by prompt tag — the analytics half of prompt-version attribution. Events stamped
/// with `metadata.prompt = "<name>@v<version>"` (the `tag` that `GET .../prompts/:name` hands every
/// client) roll up here, so "did v4 cost less than v3 in production?" is one request. Untagged
/// traffic groups under a `null` key. Window defaults to the last 30 days.
pub(crate) async fn get_prompt_costs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CostsParams>,
) -> Result<Json<Vec<lighttrack_core::CostByDimension>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let until = parse_opt_ts("until", q.until.as_deref())?.unwrap_or_else(Utc::now);
    let since =
        parse_opt_ts("since", q.since.as_deref())?.unwrap_or(until - chrono::Duration::days(30));
    let store = st.store.clone();
    let mut rows =
        spawn_db(move || store.cost_by_dimension(project.as_deref(), "prompt", since, until))
            .await?;
    rows.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
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
    let since = parse_opt_ts("since", q.since.as_deref())?;
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
    let not_found = || ApiError::not_found(format!("event '{id}' not found"));
    let ev = spawn_db(move || store.get_event(&id2))
        .await?
        .ok_or_else(not_found)?;
    // Another project's event answers exactly like a missing one. A distinct 403 would let any
    // project key probe which ids exist on the instance — a cross-tenant existence oracle over
    // client-chosen ids — and from a project key's point of view "not yours" and "not there" are
    // the same fact.
    if let Principal::Project { project_id, .. } = &p {
        if &ev.project_id != project_id {
            return Err(not_found());
        }
    }
    Ok(Json(ev))
}
