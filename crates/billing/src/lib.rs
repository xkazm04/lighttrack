//! Billing adapters: verify a provider's signed webhook and normalize it into core
//! [`lighttrack_core::RevenueEvent`]s for profit/margin tracking.
//!
//! The trait stays free of any HTTP framework — the API passes the raw request body and the value of
//! the provider's signature header, so the same adapter is reusable from a webhook handler or a poll
//! loop. Stripe ships today (`stripe`); Polar slots in behind the same [`BillingSource`] trait.

mod error;
pub mod fx;
mod registry;
pub mod polar;
pub mod stripe;

pub use error::BillingError;
pub use fx::{shared_fx, FxTable, UsdAmount};
pub use registry::BillingRegistry;

use lighttrack_core::RevenueEvent;

/// A pluggable billing provider. Verifies an inbound webhook (the provider's signature is the auth)
/// and normalizes it into zero or more revenue records.
pub trait BillingSource: Send + Sync {
    /// Provider key, e.g. `stripe` | `polar`.
    fn provider(&self) -> &'static str;

    /// Verify the webhook and normalize it. `header` looks a request header up by name
    /// (case-insensitive) — providers read whichever headers they need (Stripe: one; Polar: three).
    /// `now_unix` is the current Unix time, passed in for testability + replay-tolerance. An
    /// authentic event we don't track yields an empty vec (so the caller still 200s and the provider
    /// stops retrying).
    fn verify_webhook(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        body: &[u8],
        now_unix: i64,
    ) -> Result<Vec<RevenueEvent>, BillingError>;
}
