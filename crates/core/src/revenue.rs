use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a revenue record is recognized. `amount_usd` is always a non-negative magnitude; `Refund`
/// flips its sign at recognition time, so refunds/credits reduce recognized revenue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevenueKind {
    /// Recurring subscription; amortized across `[period_start, period_end]`.
    Subscription,
    /// One-off charge recognized at `ts`.
    #[default]
    OneTime,
    /// Usage-based charge recognized at `ts`.
    Usage,
    /// Refund/credit — subtracts from recognized revenue.
    Refund,
}

impl RevenueKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RevenueKind::Subscription => "subscription",
            RevenueKind::OneTime => "one_time",
            RevenueKind::Usage => "usage",
            RevenueKind::Refund => "refund",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "subscription" => RevenueKind::Subscription,
            "usage" => RevenueKind::Usage,
            "refund" => RevenueKind::Refund,
            _ => RevenueKind::OneTime,
        }
    }
}

/// One normalized revenue record — the revenue analog of [`crate::LlmEvent`]'s cost. Synced from a
/// billing provider (Stripe/Polar) or posted by hand; `external_id` is the provider's own id, used for
/// idempotent upserts. Attributed to a customer and/or product so it can be netted against LLM cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevenueEvent {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    /// Source billing system, e.g. `stripe` | `polar` | `manual`.
    #[serde(default = "default_source")]
    pub source: String,
    /// The provider's own id for this record (invoice/charge/order) — for idempotent upsert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Billing customer this revenue is attributed to (joins to events' `metadata.customer_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    /// Billing product/feature this revenue is attributed to (joins to events' `metadata.product_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_id: Option<String>,
    /// Non-negative magnitude in USD; sign is derived from `kind` at recognition time.
    pub amount_usd: f64,
    #[serde(default = "default_currency")]
    pub currency: String,
    #[serde(default)]
    pub kind: RevenueKind,
    /// Recognition window for subscriptions; if unset the full amount is recognized at `ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_start: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_end: Option<DateTime<Utc>>,
    #[serde(default = "Utc::now")]
    pub ts: DateTime<Utc>,
}

fn default_source() -> String {
    "manual".to_string()
}

fn default_currency() -> String {
    "USD".to_string()
}
