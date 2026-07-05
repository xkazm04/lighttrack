//! Billing-provider webhook ingest (Stripe/Polar).
//!
//! Deliberately **unauthenticated** in the bearer-key sense — the provider's HMAC signature *is* the
//! auth, verified by the configured [`lighttrack_billing::BillingSource`]. The LightTrack project is
//! taken from `?project=` on the webhook URL (configure one endpoint per project in the provider).

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct WebhookParams {
    project: Option<String>,
}

pub(crate) async fn post_webhook(
    State(st): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<WebhookParams>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let source = st
        .billing
        .get(&provider)
        .ok_or_else(|| ApiError::not_found(format!("billing provider '{provider}' is not configured")))?;
    let project = q
        .project
        .ok_or_else(|| ApiError::bad_request("webhook URL must include ?project=<id>"))?;

    let now = chrono::Utc::now().timestamp();
    let lookup = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut events = source
        .verify_webhook(&lookup, &body, now)
        .map_err(|e| ApiError::unauthorized(e.to_string()))?;
    for ev in &mut events {
        ev.project_id = project.clone();
    }

    // Validate the target project exists up front (404 if not). A typo in ?project= would otherwise
    // store valid revenue against a phantom project, where margin reports silently ignore it and the
    // money becomes invisible until reconciliation. Reject before we mark the delivery as seen, so a
    // corrected reconfiguration can be replayed.
    {
        let store = st.store.clone();
        let project_id = project.clone();
        if spawn_db(move || store.get_project(&project_id)).await?.is_none() {
            return Err(ApiError::not_found(format!(
                "project '{project}' does not exist (check the webhook URL's ?project=)"
            )));
        }
    }

    // Webhook-level idempotency, keyed on the provider's delivery id (Polar / Standard Webhooks send
    // it as the authenticated `webhook-id` header). A redelivery we've already handled is acked
    // without re-running the store writes. Providers that don't send a delivery id fall through to
    // the deterministic `revenue_events.id` upsert, which is idempotent on its own.
    let idem_key = lookup("webhook-id").map(|id| format!("{provider}:{id}"));
    if let Some(key) = &idem_key {
        if st.seen_webhooks.check_and_insert(key) {
            eprintln!("[BILLING] {provider} webhook: duplicate delivery {key}, already handled");
            return Ok(StatusCode::OK);
        }
    }

    let store = st.store.clone();
    let n = events.len();
    // Provider record ids, captured before `events` moves into the blocking closure, so a rejected
    // batch can be dead-lettered with enough to reconcile (fall back to the deterministic id).
    let provider_ids: Vec<String> = events
        .iter()
        .map(|ev| ev.external_id.clone().unwrap_or_else(|| ev.id.clone()))
        .collect();
    // One atomic transaction: the batch is all-or-nothing, so a mid-batch constraint failure can't
    // commit a partial prefix and strand the tail.
    let stored = spawn_db(move || store.insert_revenue_events(&events)).await;

    if let Err(e) = &stored {
        // The batch rolled back — nothing was persisted. Drop the idempotency mark so the provider's
        // retry is reprocessed rather than swallowed as a "duplicate", and dead-letter the rejected
        // delivery (provider ids + error) so the loss is loud and recoverable instead of surfacing at
        // month-end reconciliation.
        if let Some(key) = &idem_key {
            st.seen_webhooks.forget(key);
        }
        eprintln!(
            "[BILLING] {provider} webhook REJECTED for project={project}: {e} \
             — {n} record(s) not stored, provider ids: {provider_ids:?}"
        );
    }
    stored?;

    eprintln!("[BILLING] {provider} webhook: stored {n} revenue record(s) for project={project}");
    Ok(StatusCode::OK)
}
