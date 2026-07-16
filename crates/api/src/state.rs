//! Shared application state + the blocking-DB call helper.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use lighttrack_billing::BillingRegistry;
use lighttrack_core::{PriceBook, Redaction};
use lighttrack_store::{Store, StoreError};

use crate::alerts::Alerter;
use crate::auth::AuthMode;
use crate::collective::Collective;
use crate::error::ApiError;
use crate::idempotency::SeenWebhooks;
use crate::redact::Redactor;
use crate::rejections::RejectionLedger;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store: Arc<dyn Store + Send + Sync>,
    /// DB-backed price book, hot-swappable via `PUT /v1/prices/:provider/:model`.
    pub(crate) prices: Arc<RwLock<PriceBook>>,
    pub(crate) auth_mode: AuthMode,
    pub(crate) admin_key: Option<String>,
    /// The enrolled local device's bearer key for the relay lease/result endpoints
    /// (`LIGHTTRACK_RELAY_DEVICE_KEY`). Unset ⇒ only admin/dev principals may drive them.
    pub(crate) relay_device_key: Option<String>,
    /// Fixed per-request cost stamped on relay-run events (`LIGHTTRACK_RELAY_FLAT_COST_USD`,
    /// default $1.00). Subscription runs have no metered price; a flat rate gives a solid usage
    /// overview until token-precise costing is worth wiring up (docs/RELAY.md).
    pub(crate) relay_flat_cost: f64,
    /// Best-effort breach-alert delivery (webhook / ntfy), configured from env.
    pub(crate) alerts: Arc<Alerter>,
    /// Optional PII redaction of captured input/output on ingest, configured from env.
    pub(crate) redact: Arc<Redactor>,
    /// Per-project payload-persistence policies (the stored `Project.redaction` field), cached so the
    /// ingest hot path doesn't pay a store read per event. Warmed from `list_projects()` at startup,
    /// updated on project create, and back-filled lazily on first sight of an unknown project (see
    /// [`redaction_policy_for`]). Single-API-instance authoritative — matching the store's
    /// one-writer architecture; a multi-replica deploy would need a TTL or invalidation bus.
    pub(crate) redaction_policies: Arc<RwLock<HashMap<String, Redaction>>>,
    /// Configured billing-webhook sources (Stripe/Polar), keyed by provider.
    pub(crate) billing: Arc<BillingRegistry>,
    /// In-process idempotency for webhook deliveries — collapses provider retries / duplicate
    /// deliveries of the same event so they aren't reprocessed (durable backstop: deterministic
    /// `revenue_events.id` upsert).
    pub(crate) seen_webhooks: Arc<SeenWebhooks>,
    /// Collective Model Intelligence config (opaque contributor id + hub accept flag), from env.
    pub(crate) collective: Arc<Collective>,
    /// Best-effort, process-local ledger of ingest attempts that limit rules rejected (429). Rejected
    /// events are deliberately never stored (they'd corrupt usage/cost), so this counts them out-of-band
    /// so history isn't blind exactly when a cap bites. Resets on restart; entries roll off after 24h.
    pub(crate) rejections: Arc<RejectionLedger>,
}

/// The payload-persistence policy for `pid`, from the cache — falling back to one store read on the
/// first sight of a project the cache doesn't know (then remembered, including the "no project row"
/// default, so the miss is paid once per project per process). This is what makes the stored
/// `Project.redaction` field an *enforced* policy on ingest instead of a decorative column.
pub(crate) async fn redaction_policy_for(st: &AppState, pid: &str) -> Result<Redaction, ApiError> {
    if let Some(p) = st.redaction_policies.read().unwrap().get(pid) {
        return Ok(*p);
    }
    let store = st.store.clone();
    let id = pid.to_string();
    let policy = spawn_db(move || store.get_project(&id))
        .await?
        .map(|p| p.redaction)
        .unwrap_or_default();
    st.redaction_policies.write().unwrap().insert(pid.to_string(), policy);
    Ok(policy)
}

/// Run a blocking store call on the blocking pool and flatten the two error layers.
pub(crate) async fn spawn_db<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("task join error: {e}")))?
        .map_err(ApiError::from)
}
