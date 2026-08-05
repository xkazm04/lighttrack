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

use lighttrack_core::{normalize_trace_ref, LimitStatus, LlmEvent, Status};
use lighttrack_store::{Admission, CostRow, EventFilter, StoreError, UseCaseCostRow};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::events_validate::{policy, Rejection};
use crate::guards::{authenticate, resolve_ingest_project_ensuring, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// Scope one event to its project, validate it, enforce the project's payload-persistence policy,
/// scrub PII, and fill/mark its cost — everything the single- and batch-ingest paths share up to the
/// admission step. On a validation failure returns a coded [`Rejection`] (the batch path records its
/// code per item; the single path turns it into the matching HTTP error).
///
/// This is also where server trust is established: `received_at` is stamped from the server clock and
/// `metadata.api_key_id` from the authenticated principal, both overwriting anything that reached the
/// struct — so neither the timestamp every rolling window is measured on nor the identity a per-key
/// budget is charged against is something the client can influence.
pub(crate) fn prepare_event(
    st: &AppState,
    ev: &mut LlmEvent,
    pid: &str,
    key_id: Option<&str>,
    persistence: lighttrack_core::Redaction,
) -> Result<(), Rejection> {
    let now = Utc::now();
    ev.project_id = pid.to_string();
    ev.received_at = now;
    normalize_ids(ev);
    stamp_api_key(ev, key_id);
    policy().validate(ev, now)?;
    // Both redaction lines below are `debug`, not `info`: they fire once per ingested event, and with
    // the PII scrub now on by default (D14) an `info` line per event would be the single loudest thing
    // in the log. That redaction is running belongs in the startup posture line, not in a per-event
    // repeat; at `debug` they are still there when you are chasing a mangled payload.
    //
    // 1. The project's stored persistence policy (hash/drop) — the setting the projects API accepts
    // and the operator table displays, now actually enforced. Applied before the PII scrub: `drop`
    // removes the payloads outright; `hash` leaves nothing scrubbable.
    if crate::redact::apply_policy(ev, persistence) {
        tracing::debug!(project_id = %pid, event_id = %ev.id, policy = ?persistence, "applied payload persistence policy");
    }
    // 2. Env-configured floor: scrub structured PII from what remains before it is stored.
    let redacted = st.redact.redact_event(ev, persistence);
    if redacted > 0 {
        tracing::debug!(project_id = %pid, event_id = %ev.id, spans = redacted, "scrubbed PII spans from an ingested event");
    }
    let client_supplied = ev.cost_usd.is_some();
    {
        let book = st.prices.read().unwrap();
        ev.ensure_cost(&book);
    }
    mark_cost_source(ev, client_supplied);
    Ok(())
}

/// Canonicalize the event's trace/span ids. Both front doors pass through here, so a W3C hex id is
/// case-folded identically whether it arrived over OTLP (which already lower-cased) or from an SDK
/// (which normalized nothing) — that mismatch is what split one end-to-end trace spanning an OTel
/// service and an SDK service into two. A non-W3C id (`"req-1"`) is left verbatim; see
/// [`normalize_trace_ref`].
fn normalize_ids(ev: &mut LlmEvent) {
    for id in [&mut ev.trace_id, &mut ev.span_id, &mut ev.parent_span_id] {
        if let Some(v) = id.as_deref() {
            *id = Some(normalize_trace_ref(v));
        }
    }
}

/// Stamp the authenticated API key's **id** onto the event as `metadata.api_key_id`, and — this is
/// the load-bearing half — **remove** any `api_key_id` the client sent when there is no key behind
/// the request (admin/dev principals, or a keyless dev-mode call).
///
/// Without the removal a caller could simply write `{"api_key_id": "<the-other-key>"}` and either
/// launder its spend onto another key's budget or dodge its own per-key cap; attribution would name
/// whoever the attacker chose. The field is therefore server-owned in exactly the way `received_at`
/// is: read from the principal, never from the body.
///
/// What is persisted is the opaque `api_keys.id`. Not the presented token, not its prefix, not a
/// hash of it — nothing that could be replayed or reversed if an event row leaks. Rows written before
/// this existed simply carry no `api_key_id` and fall into the unattributed bucket.
fn stamp_api_key(ev: &mut LlmEvent, key_id: Option<&str>) {
    match (&mut ev.metadata, key_id) {
        (Value::Object(m), Some(id)) => {
            m.insert("api_key_id".to_string(), Value::String(id.to_string()));
        }
        (Value::Object(m), None) => {
            m.remove("api_key_id");
        }
        (v, Some(id)) if v.is_null() => {
            *v = Value::Object(
                [("api_key_id".to_string(), Value::String(id.to_string()))]
                    .into_iter()
                    .collect(),
            );
        }
        // Null metadata with no key, or non-object metadata (client-owned scalar/array — it can hold
        // no `api_key_id` to forge): nothing to do.
        _ => {}
    }
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
        v @ Value::Null => {
            *v = Value::Object([("cost_source".to_string(), src)].into_iter().collect())
        }
        _ => {} // non-object, non-null metadata is client-owned: don't clobber it
    }
}

/// Post-admission side effects shared by the single- and batch-ingest paths: log and best-effort
/// deliver breach alerts, count a rejected event into the rejection ledger, and (for an admitted
/// non-success call) feed error-spike detection. Returns the breached statuses so the caller can
/// shape its response (429 vs. observe-only flag).
pub(crate) fn on_admission(
    st: &AppState,
    ev: &LlmEvent,
    admission: &Admission,
) -> Vec<LimitStatus> {
    let breached: Vec<LimitStatus> = admission
        .statuses
        .iter()
        .filter(|s| s.breached)
        .cloned()
        .collect();
    for b in &breached {
        tracing::warn!(
            project_id = %b.project_id,
            metric = ?b.metric,
            window = ?b.window,
            value = b.current,
            threshold = b.threshold,
            action = ?b.action,
            "usage limit breached",
        );
    }
    // A rejected event is never stored (that would corrupt usage/cost), so count it out-of-band in the
    // best-effort rejection ledger — the running per-key count then rides along on the breach alert.
    // Its estimated cost is the priced `cost_usd` if we resolved one, else $0 (unpriced).
    // Rejection is not always a breach: an enforcing cost cap whose window cannot be priced at all
    // refuses ingest without any status reading "breached", so the ledger is fed from every status
    // that rejects, not just the breached ones.
    let rej_counts = if admission.admitted {
        std::collections::HashMap::new()
    } else {
        record_rejection(st, ev, &admission.statuses)
    };
    for s in admission.statuses.iter().filter(|s| s.shedding) {
        tracing::info!(
            project_id = %s.project_id,
            metric = ?s.metric,
            window = ?s.window,
            ratio = s.ratio,
            shed_pct = s.shed_fraction * 100.0,
            event_id = %ev.id,
            "throttling ingest: graduated back-pressure, not a breach",
        );
    }
    // Best-effort, off the request path: deliver breaches to webhook/ntfy (deduped per cooldown).
    st.alerts.notify(&breached, &rej_counts);
    // Soft-warning tier: for an *admitted* event, alert on any rule that crossed its warn_at without
    // breaching — the operator's early heads-up before the cap actually bites. Only when admitted, so
    // the usage the warning reports genuinely includes a recorded event (a rejected event isn't stored).
    if admission.admitted {
        let warnings: Vec<LimitStatus> = admission
            .statuses
            .iter()
            .filter(|s| s.warning)
            .cloned()
            .collect();
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
    statuses: &[LimitStatus],
) -> std::collections::HashMap<String, u64> {
    let cost = ev.cost_usd.unwrap_or(0.0);
    let now = Utc::now();
    let mut counts = std::collections::HashMap::new();
    // Every status that turned this event away, hard stop or graduated shed alike — otherwise the
    // ledger would go blind exactly while throttling is doing its job.
    for b in statuses.iter().filter(|s| s.rejects_ingest() || s.shedding) {
        let count = st.rejections.record(
            &b.project_id,
            b.metric,
            b.window,
            b.scope.clone(),
            cost,
            now,
        );
        counts.insert(b.alert_key(), count);
    }
    counts
}

/// Human-facing reason an admission was rejected — pass the full status set, since neither an
/// unpriceable cost cap nor a graduated throttle shed reads as "breached".
pub(crate) fn breach_reason(statuses: &[LimitStatus]) -> String {
    // A graduated shed is not a breach and must not be described as one: nothing is over budget, the
    // caller is being asked to slow down on the approach. Only reported when no hard stop applies.
    if !statuses.iter().any(|s| s.rejects_ingest()) {
        if let Some(s) = statuses.iter().find(|s| s.shedding) {
            let scope = match &s.scope {
                Some(sc) => format!(" [scope {}]", sc.label()),
                None => String::new(),
            };
            return format!(
                "ingest throttled: project '{}'{scope} is at {:.0}% of its {:?}/{:?} limit \
                 ({:.4} of {:.4}); {:.0}% of ingest is being shed on the approach. Not over budget — \
                 slow down and retry in {}s.",
                s.project_id,
                s.ratio * 100.0,
                s.metric,
                s.window,
                s.current,
                s.threshold,
                s.shed_fraction * 100.0,
                s.retry_after_secs()
            );
        }
    }
    statuses
        .iter()
        .find(|s| s.rejects_ingest())
        .map(|s| {
            let scope = match &s.scope {
                Some(sc) => format!(" [scope {}]", sc.label()),
                None => String::new(),
            };
            if s.unpriceable() && !s.breached {
                // The distinct, visible condition: we are not over budget, we simply cannot measure
                // the budget. Say exactly that, and how to fix it.
                return format!(
                    "ingest blocked: project '{}'{scope} has an enforcing {:?}/{:?} cost limit but \
                     no priced traffic in the window — this model is absent from the price book, so \
                     the cap cannot be measured. Add a price for it (POST /v1/prices) or cap on \
                     calls/tokens instead.",
                    s.project_id, s.metric, s.window
                );
            }
            let estimated = if s.estimated() { " (includes imputed cost for unpriced calls)" } else { "" };
            format!(
                "ingest blocked: project '{}'{scope} is over its {:?}/{:?} limit \
                 ({:.4} >= {:.4}, action={:?}){estimated}",
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
    /// `true` when this request was a replay of an already-recorded event (same id, same logical
    /// payload): the original outcome is returned and nothing is double-counted, so a client may
    /// retry a timed-out POST safely. Omitted (false) on first-time writes.
    #[serde(skip_serializing_if = "is_false")]
    duplicate: bool,
    /// **Proximity signal.** The highest usage ratio among the limits that applied to this event
    /// (`1.0` == at the cap). Returned on *accepted* writes so a well-behaved client can see the wall
    /// coming without polling `/v1/limits/status`. `None` when the project has no limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_ratio: Option<f64>,
    /// Share of ingest currently being shed by graduated throttling, `0.0`–`1.0`. Omitted when
    /// nothing is throttling. A client seeing this rise should slow down: at `1.0` the cap is reached
    /// and everything is refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    shed_fraction: Option<f64>,
}

/// The proximity pair returned on an accepted write: the worst usage ratio across the rules that
/// applied, and the strongest shedding pressure among them.
fn proximity(statuses: &[LimitStatus]) -> (Option<f64>, Option<f64>) {
    let ratio = statuses
        .iter()
        .map(|s| s.ratio)
        .fold(None::<f64>, |a, r| Some(a.map_or(r, |a| a.max(r))));
    let shed = statuses
        .iter()
        .map(|s| s.shed_fraction)
        .fold(None::<f64>, |a, r| Some(a.map_or(r, |a: f64| a.max(r))))
        .filter(|f| *f > 0.0);
    (ratio, shed)
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Whether a stored event and an incoming write are the same **logical** event — the replay test for
/// a retried request. Compares the identity scalars (project, client-supplied ts, provider/model,
/// token counts) and deliberately NOT `cost_usd` (the price book may have changed between attempts)
/// or `input`/`output` (the redaction policy may have). A PK collision that passes this is a client
/// retry to acknowledge; one that fails it is a true id conflict to refuse.
pub(crate) fn same_logical_event(stored: &LlmEvent, incoming: &LlmEvent) -> bool {
    stored.project_id == incoming.project_id
        && stored.ts == incoming.ts
        && stored.provider == incoming.provider
        && stored.model == incoming.model
        && stored.usage.input == incoming.usage.input
        && stored.usage.output == incoming.usage.output
}

pub(crate) async fn post_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut ev): Json<LlmEvent>,
) -> Result<Json<IngestResponse>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    let pid = resolve_ingest_project_ensuring(&st, &principal, &ev.project_id).await?;
    let persistence = crate::state::redaction_policy_for(&st, &pid).await?;
    prepare_event(&st, &mut ev, &pid, principal.key_id(), persistence)?;

    // Admission control: evaluate the project's limits and insert in one atomic store step. An
    // enforcing (Throttle/Block) breach rejects the event — it is NOT recorded and we return 429 so
    // a cooperating client backs off. This is what makes a configured cap an actual cap, not a flag.
    let store = st.store.clone();
    let to_insert = ev.clone();
    let inserted = tokio::task::spawn_blocking(move || store.insert_event_checked(&to_insert))
        .await
        .map_err(|e| ApiError::internal(format!("task join error: {e}")))?;
    let admission = match inserted {
        Ok(a) => a,
        // A PK collision is a RETRY until proven otherwise: a client whose POST timed out after the
        // server committed must be able to resend and learn "you already have this, all good" —
        // previously this was a bare 409, indistinguishable from "malformed and gone", which is why
        // the SDK couldn't retry at all. Same logical payload → acknowledge the original write
        // (nothing double-counted); different payload under the same id → a true conflict, still 409.
        Err(StoreError::Conflict(_)) => {
            let store = st.store.clone();
            let id = ev.id.clone();
            let stored = spawn_db(move || store.get_event(&id)).await?;
            return match stored {
                Some(s) if same_logical_event(&s, &ev) => Ok(Json(IngestResponse {
                    id: ev.id,
                    project_id: pid,
                    cost_usd: s.cost_usd,
                    ts: s.ts,
                    breached: Vec::new(),
                    throttled: false,
                    duplicate: true,
                    usage_ratio: None,
                    shed_fraction: None,
                })),
                _ => Err(ApiError::conflict(format!(
                    "event '{}' already exists with a different payload",
                    ev.id
                ))),
            };
        }
        Err(e) => return Err(e.into()),
    };

    let breached = on_admission(&st, &ev, &admission);
    if !admission.admitted {
        // 429 for both tiers, but with a retry schedule the client can actually honor: seconds for a
        // graduated shed, the window's own back-off for a hard cap.
        return Err(ApiError::rate_limited(breach_reason(&admission.statuses))
            .retry_after(admission.retry_after_secs));
    }

    // Admitted: any remaining breaches are Alert-only (enforcing ones would have 429'd above).
    let throttled = breached.iter().any(|s| s.rejects_ingest());
    let (usage_ratio, shed_fraction) = proximity(&admission.statuses);
    Ok(Json(IngestResponse {
        id: ev.id,
        project_id: pid,
        cost_usd: ev.cost_usd,
        ts: ev.ts,
        breached,
        throttled,
        duplicate: false,
        usage_ratio,
        shed_fraction,
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
    /// When `1`/`true`, also return the total number of matching events in `X-Total-Count`. Opt-in:
    /// it costs a second aggregate query. Taken as a string because a query string carries `1`/`0`
    /// as often as `true`/`false`, and a strict bool parse would 400 on the common form.
    count: Option<String>,
    /// Opaque keyset cursor from a prior page's `X-Next-Cursor` header.
    cursor: Option<String>,
    /// When `1`/`true`, return only the most recent events that do not yet have a score (the online
    /// scorer's work list). Uses a scoped anti-join, so it stays correct however large `scores` grows;
    /// ignores the filter/cursor params (project + limit only).
    unscored: Option<bool>,
}

/// Whether a query-string flag reads as set: `1` / `true` / `yes` (case-insensitive).
fn is_truthy(v: Option<&str>) -> bool {
    matches!(
        v.map(str::trim),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
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
        if !["success", "error", "timeout"].contains(&s) {
            return Err(ApiError::bad_request(format!(
                "invalid 'status' {s:?}: expected success | error | timeout"
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
        with_total: is_truthy(q.count.as_deref()),
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
    if let Principal::Project {
        project_id: pid, ..
    } = &p
    {
        if &ev.project_id != pid {
            return Err(ApiError::forbidden(
                "key not authorized for that event's project",
            ));
        }
    }
    Ok(Json(ev))
}
