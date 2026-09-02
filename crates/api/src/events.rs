//! Event ingest: the prepare pipeline both doors share, and `POST /v1/events`. Reads live in
//! `events_query`, post-admission side effects in `events_admission`.

use axum::{extract::State, http::HeaderMap, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use lighttrack_core::{normalize_trace_ref, LimitStatus, LlmEvent};
use lighttrack_store::StoreError;

use crate::error::ApiError;
use crate::events_admission::{breach_reason, on_admission};
use crate::events_validate::{policy, Rejection};
use crate::guards::{authenticate, resolve_ingest_project_ensuring};
use crate::ingest_proximity::{BindingScope, Proximity, WithProximity};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

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
    // An absent `id` is minted by the deserializer; an explicit `""` sailed past it and became the
    // primary key `""` — so the second such event in a project was a 409 against the first. Blank
    // means "you choose", same as absent.
    if ev.id.trim().is_empty() {
        ev.id = lighttrack_core::new_id();
    }
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
        // A poisoned lock (a writer panicked mid-swap) must not take every subsequent ingest down
        // with it: the book is replaced wholesale, never mutated in place, so whatever is under the
        // lock is a complete, usable snapshot.
        let book = st.prices.read().unwrap_or_else(|p| p.into_inner());
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
    match key_id {
        Some(id) => metadata_set(ev, "api_key_id", Value::String(id.to_string())),
        // Non-object metadata (a client-owned scalar/array) can hold no `api_key_id` to forge, so
        // only an object needs the strip.
        None => {
            if let Value::Object(m) = &mut ev.metadata {
                m.remove("api_key_id");
            }
        }
    }
}

/// Set one server-owned key in `metadata`, creating the object when metadata is null. Non-object,
/// non-null metadata is a client-owned scalar/array: it is left untouched rather than clobbered.
fn metadata_set(ev: &mut LlmEvent, key: &str, value: Value) {
    match &mut ev.metadata {
        Value::Object(m) => {
            m.insert(key.to_string(), value);
        }
        v @ Value::Null => *v = Value::Object([(key.to_string(), value)].into_iter().collect()),
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
    metadata_set(ev, "cost_source", src);
}

#[derive(Serialize)]
pub(crate) struct IngestResponse {
    id: String,
    project_id: String,
    cost_usd: Option<f64>,
    ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    breached: Vec<LimitStatus>,
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
    /// **Which** rule the `usage_ratio` belongs to, as `{kind, value}` — `None` when the binding
    /// rule is project-wide. `0.94` alone tells a client to stop everything; `0.94` on
    /// `model=gpt-4o` tells it to route the next call elsewhere and keep working.
    #[serde(skip_serializing_if = "Option::is_none")]
    binding_scope: Option<BindingScope>,
    /// Id of the binding rule, so a client can reproduce the server's own shed decision (§7c hashes
    /// `(rule_id, event_id)`). Omitted when the project has no limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    binding_rule: Option<String>,
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

/// The refusal both ingest doors return for a project whose `enabled` flag is off. Names the fix:
/// the flag is set through the same endpoint that cleared it.
pub(crate) fn disabled_project_msg(pid: &str) -> String {
    format!(
        "project '{pid}' is disabled: ingest refused. Re-enable it with \
         PUT /v1/projects/{pid} {{\"enabled\": true}}"
    )
}

pub(crate) async fn post_event(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut ev): Json<LlmEvent>,
) -> Result<WithProximity<IngestResponse>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    let pid = resolve_ingest_project_ensuring(&st, &principal, &ev.project_id).await?;
    let policy = crate::state::project_policy_for(&st, &pid).await?;
    if !policy.enabled {
        return Err(ApiError::project_disabled(disabled_project_msg(&pid)));
    }
    prepare_event(&st, &mut ev, &pid, principal.key_id(), policy.redaction)?;

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
            let owner = ev.project_id.clone();
            let stored =
                spawn_db(move || store.get_event(TenantScope::Project(&owner), &id)).await?;
            return match stored {
                Some(s) if same_logical_event(&s, &ev) => Ok(WithProximity::new(
                    IngestResponse {
                        id: ev.id,
                        project_id: pid,
                        cost_usd: s.cost_usd,
                        ts: s.ts,
                        breached: Vec::new(),
                        duplicate: true,
                        usage_ratio: None,
                        shed_fraction: None,
                        binding_scope: None,
                        binding_rule: None,
                    },
                    // A replay is answered from the stored row without re-running admission, so
                    // there is no position to report. Silence beats a stale number.
                    Proximity::default(),
                )),
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
        let mut prox = Proximity::of(&admission.statuses);
        prox.retry_after_secs = admission.retry_after_secs;
        return Err(ApiError::rate_limited(breach_reason(&admission.statuses))
            .retry_after(admission.retry_after_secs)
            .proximity(prox));
    }

    // Admitted: any remaining breaches are Alert-only (enforcing ones would have 429'd above), so
    // the response carries them as observe-only detail — there is no separate "throttled" flag to
    // report, because an admitted event is by definition one nothing enforcing turned away.
    let prox = Proximity::of(&admission.statuses);
    Ok(WithProximity::new(
        IngestResponse {
            id: ev.id,
            project_id: pid,
            cost_usd: ev.cost_usd,
            ts: ev.ts,
            breached,
            duplicate: false,
            usage_ratio: prox.usage_ratio,
            shed_fraction: prox.shed_fraction,
            binding_scope: prox.binding_scope.clone(),
            binding_rule: prox.binding_rule.clone(),
        },
        prox,
    ))
}
