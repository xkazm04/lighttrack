//! Shared application state + the blocking-DB call helper.

use std::sync::{Arc, RwLock};

use lighttrack_billing::BillingRegistry;
use lighttrack_core::PriceBook;
use lighttrack_store::{Store, StoreError};

use crate::alerts::Alerter;
use crate::auth::AuthMode;
use crate::collective::Collective;
use crate::error::ApiError;
use crate::idempotency::SeenWebhooks;
use crate::redact::Redactor;

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
    /// Configured billing-webhook sources (Stripe/Polar), keyed by provider.
    pub(crate) billing: Arc<BillingRegistry>,
    /// In-process idempotency for webhook deliveries — collapses provider retries / duplicate
    /// deliveries of the same event so they aren't reprocessed (durable backstop: deterministic
    /// `revenue_events.id` upsert).
    pub(crate) seen_webhooks: Arc<SeenWebhooks>,
    /// Collective Model Intelligence config (opaque contributor id + hub accept flag), from env.
    pub(crate) collective: Arc<Collective>,
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
