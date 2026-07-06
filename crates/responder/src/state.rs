//! Shared axum state: the immutable config plus the auto-fix circuit breaker.

use std::sync::Arc;

use crate::breaker::Breaker;
use crate::config::Config;

#[derive(Clone)]
pub(crate) struct AppState {
    pub cfg: Arc<Config>,
    pub breaker: Arc<Breaker>,
}
