//! Shared application state + the blocking-DB call helper.

use std::sync::{Arc, RwLock};

use lighttrack_core::PriceBook;
use lighttrack_store::{Store, StoreError};

use crate::alerts::Alerter;
use crate::auth::AuthMode;
use crate::error::ApiError;
use crate::redact::Redactor;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store: Arc<dyn Store + Send + Sync>,
    /// DB-backed price book, hot-swappable via `PUT /v1/prices/:provider/:model`.
    pub(crate) prices: Arc<RwLock<PriceBook>>,
    pub(crate) auth_mode: AuthMode,
    pub(crate) admin_key: Option<String>,
    /// Best-effort breach-alert delivery (webhook / ntfy), configured from env.
    pub(crate) alerts: Arc<Alerter>,
    /// Optional PII redaction of captured input/output on ingest, configured from env.
    pub(crate) redact: Arc<Redactor>,
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
