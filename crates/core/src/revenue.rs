use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a revenue record is recognized. `amount_usd` is always a non-negative magnitude; `Refund`
/// flips its sign at recognition time, so refunds/credits reduce recognized revenue.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// The provider's own figure, in the currency's **minor unit** (Stripe `amount_paid`, Polar
    /// `total_amount`). The one number on this row that never needs restating: `amount_usd` is a
    /// derived value and a wrong rate makes it wrong, but ¥5000 was ¥5000 whatever the book said.
    /// Keeping it is what makes [`crate::RevenueEvent`] repriceable instead of re-ingestible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_minor: Option<i64>,
    /// USD per one major unit of `currency` at the time of conversion. `1.0` for the base currency;
    /// `None` on a row written before FX provenance existed, or on the 1:1 fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_rate: Option<f64>,
    /// Which FX book produced `fx_rate` ([`crate::revenue`]'s caller passes
    /// `lighttrack_billing::FxTable::version`). Without it, "we fixed the EUR rate" and "these rows
    /// already had the fixed rate" are the same sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_book_version: Option<String>,
    /// Whether a real conversion happened. `Some(false)` is the **1:1 fallback**: no rate existed,
    /// so the major-unit figure was stored as if it were USD and is approximate. `None` is a row
    /// that predates the field — read it through [`RevenueEvent::is_converted`], never as `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted: Option<bool>,
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

/// The reporting base every stored `amount_usd` is denominated in. Only used to read a row that
/// predates [`RevenueEvent::converted`]; the live table's base lives in `lighttrack_billing`.
const REPORTING_BASE: &str = "USD";

impl RevenueEvent {
    /// Did this row's amount go through a real conversion?
    ///
    /// A row written before FX provenance existed carries no answer, and the honest inference is the
    /// one the old code path guaranteed: a base-currency amount needed no rate, so it converted;
    /// anything else may or may not have. Inferring rather than defaulting to `false` keeps the
    /// margin caveat from suddenly flagging every historical USD invoice as approximate.
    ///
    /// Deliberately **not** re-derived from the live FX table: that is the bug this replaces. The
    /// caveat used to be recomputed per request against the current book, so adding a missing rate
    /// later made the warning disappear while the rows stored at 1:1 stayed wrong.
    pub fn is_converted(&self) -> bool {
        self.converted
            .unwrap_or_else(|| self.currency.eq_ignore_ascii_case(REPORTING_BASE))
    }
}

fn default_source() -> String {
    "manual".to_string()
}

fn default_currency() -> String {
    "USD".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(currency: &str, converted: Option<bool>) -> RevenueEvent {
        serde_json::from_value(serde_json::json!({
            "amount_usd": 1.0, "currency": currency, "converted": converted
        }))
        .expect("row")
    }

    /// The stamp wins when present; a row that predates it is inferred from its currency, never
    /// defaulted to "unconverted" - or every historical USD invoice would read as approximate.
    #[test]
    fn conversion_is_read_from_the_stamp_and_inferred_only_when_absent() {
        assert!(row("EUR", Some(true)).is_converted());
        assert!(
            !row("USD", Some(false)).is_converted(),
            "an explicit fallback stamp is honoured"
        );
        assert!(
            row("usd", None).is_converted(),
            "a pre-stamp USD row needed no rate"
        );
        assert!(
            !row("GBP", None).is_converted(),
            "a pre-stamp GBP row may or may not have converted"
        );
    }

    #[test]
    fn kind_wire_strings_round_trip() {
        for k in [
            RevenueKind::Subscription,
            RevenueKind::OneTime,
            RevenueKind::Usage,
            RevenueKind::Refund,
        ] {
            assert_eq!(RevenueKind::parse(k.as_str()), k);
            assert_eq!(
                serde_json::to_value(k).expect("kind"),
                serde_json::Value::String(k.as_str().to_string())
            );
        }
        assert_eq!(RevenueKind::parse("nonsense"), RevenueKind::OneTime);
    }
}
