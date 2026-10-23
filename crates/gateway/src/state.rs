//! Shared handler state.

use std::sync::Arc;
use std::time::Duration;

use crate::admission::Admission;
use crate::config::GatewayConfig;
use crate::cooldown::Cooldowns;
use crate::generator::Generator;
use crate::telemetry::Telemetry;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<GatewayConfig>,
    pub gen: Arc<dyn Generator>,
    pub cooldowns: Arc<Cooldowns>,
    /// `None` when no API is configured: the gateway still routes and fails over, it just
    /// records nothing and admits everything.
    pub telemetry: Option<Arc<Telemetry>>,
    pub admission: Arc<Admission>,
    /// `LIGHTTRACK_GATEWAY_DEV=1`: honour `X-LightTrack-Simulate: exhausted:<provider>`, which
    /// lets an onboarding run prove the failover path without waiting for a real limit.
    pub dev: bool,
}

impl AppState {
    pub fn default_hold(&self) -> Duration {
        Duration::from_secs(self.cfg.cooldown_secs)
    }
}
