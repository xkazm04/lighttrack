//! Pricing what-if simulator — recompute margin under a *hypothetical* price model, purely.
//!
//! All the usage data (tokens, cost) already sits in the events table; the operator just wants to ask
//! "if I charged $X per 1M tokens (or $Y/month per customer), what would my margin look like?". This is
//! a pure, I/O-free overlay: real windowed cost stays fixed ([`compute_margin`]'s cost side), and each
//! key's revenue is *replaced* by `price_per_mtok * tokens/1e6 + flat_monthly` (the flat fee prorated to
//! the window vs a 30-day month). Actual margin (from real revenue records) rides alongside so the
//! operator sees the what-if uplift as a per-key delta. Nothing here writes — it is decision support.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::margin::{compute_margin, CostByDimension, MarginDimension, UNATTRIBUTED};
use crate::revenue::RevenueEvent;

/// Days a `flat_monthly` charge is prorated against — a nominal 30-day "month".
pub const PRORATION_MONTH_DAYS: f64 = 30.0;

/// Prompt+completion tokens consumed by one billing-dimension value over the window (store-produced
/// from events). The `None` key is untagged (unattributed) usage, mirroring [`CostByDimension`].
#[derive(Debug, Clone)]
pub struct TokensByDimension {
    pub key: Option<String>,
    pub tokens: i64,
}

/// The hypothetical price model being simulated. At least one of `price_per_mtok` / `flat_monthly`
/// must be set (validated by the caller); an unset field contributes nothing. Echoed in the response.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SimAssumptions {
    /// Hypothetical charge per 1M prompt+completion tokens, applied to each key's window token count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_per_mtok: Option<f64>,
    /// Hypothetical flat charge per key per 30-day month, prorated to the window length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flat_monthly: Option<f64>,
    /// Window length in days — the proration basis, so the response is self-explanatory
    /// (`flat_monthly * window_days / 30`).
    pub window_days: f64,
}

impl SimAssumptions {
    /// Build from optional params over a window; `Err` with a reason when neither price is supplied.
    pub fn new(
        price_per_mtok: Option<f64>,
        flat_monthly: Option<f64>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Self, &'static str> {
        if price_per_mtok.is_none() && flat_monthly.is_none() {
            return Err("at least one of `price_per_mtok` or `flat_monthly` is required");
        }
        let window_days = (until - since).num_seconds() as f64 / 86_400.0;
        Ok(SimAssumptions { price_per_mtok, flat_monthly, window_days })
    }
}

/// One key's simulated-vs-actual margin — the actual side is what `/v1/margin` reports; the simulated
/// side applies [`SimAssumptions`]; `margin_delta_usd` is the what-if uplift (negative = worse).
#[derive(Debug, Clone, Serialize)]
pub struct SimRow {
    pub key: String,
    pub tokens: i64,
    pub calls: i64,
    pub llm_cost_usd: f64,
    /// Recognized revenue from real revenue records (what `/v1/margin` reports for this key).
    pub actual_revenue_usd: f64,
    pub actual_margin_usd: f64,
    /// Revenue under the hypothetical price model.
    pub simulated_revenue_usd: f64,
    pub simulated_margin_usd: f64,
    /// `simulated_margin - actual_margin` — the uplift from switching to the hypothetical model.
    pub margin_delta_usd: f64,
}

/// Recompute margin per dimension key under `assumptions`. Cost is the real windowed cost (same
/// [`compute_margin`] machinery); actual revenue/margin come from the real revenue records; the
/// simulated side replaces revenue with the hypothetical model. Rows are sorted by simulated margin
/// ascending, so the key that would still lose money surfaces first.
pub fn compute_margin_simulation(
    revenue: &[RevenueEvent],
    costs: &[CostByDimension],
    tokens: &[TokensByDimension],
    assumptions: SimAssumptions,
    dim: MarginDimension,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<SimRow> {
    // Actual margin per key from real revenue + real cost — identical machinery to `/v1/margin`. Its
    // key set is the union of revenue and cost keys, and token keys are a subset of cost keys (same
    // events, same dimension), so every token key already has a row here.
    let actual = compute_margin(revenue, costs, dim, since, until);

    let mut tok: BTreeMap<String, i64> = BTreeMap::new();
    for t in tokens {
        let key = t.key.clone().unwrap_or_else(|| UNATTRIBUTED.to_string());
        *tok.entry(key).or_default() += t.tokens;
    }

    let ppm = assumptions.price_per_mtok.unwrap_or(0.0);
    let flat = assumptions.flat_monthly.unwrap_or(0.0);
    let proration = assumptions.window_days / PRORATION_MONTH_DAYS;

    let mut rows: Vec<SimRow> = actual
        .into_iter()
        .map(|r| {
            let key_tokens = tok.get(&r.key).copied().unwrap_or(0);
            let sim_rev = hypothetical_revenue(key_tokens, ppm, flat, proration);
            let sim_margin = sim_rev - r.llm_cost_usd;
            SimRow {
                key: r.key,
                tokens: key_tokens,
                calls: r.calls,
                llm_cost_usd: r.llm_cost_usd,
                actual_revenue_usd: r.revenue_usd,
                actual_margin_usd: r.gross_margin_usd,
                simulated_revenue_usd: round(sim_rev),
                simulated_margin_usd: round(sim_margin),
                margin_delta_usd: round(sim_margin - r.gross_margin_usd),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.simulated_margin_usd.total_cmp(&b.simulated_margin_usd));
    rows
}

/// Hypothetical revenue for one key: a token-metered charge plus a prorated flat fee. Broken out so the
/// per-key formula (including proration) is unit-testable in isolation.
pub(crate) fn hypothetical_revenue(
    tokens: i64,
    price_per_mtok: f64,
    flat_monthly: f64,
    proration: f64,
) -> f64 {
    price_per_mtok * (tokens as f64 / 1_000_000.0) + flat_monthly * proration
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::revenue::RevenueKind;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn rev(customer: &str, amount: f64, kind: RevenueKind, ts: &str) -> RevenueEvent {
        RevenueEvent {
            id: "r".into(),
            project_id: "p".into(),
            source: "manual".into(),
            external_id: None,
            customer_id: Some(customer.into()),
            product_id: None,
            amount_usd: amount,
            currency: "USD".into(),
            kind,
            period_start: None,
            period_end: None,
            ts: t(ts),
        }
    }

    fn cost(customer: Option<&str>, cost_usd: f64, calls: i64) -> CostByDimension {
        CostByDimension { key: customer.map(str::to_string), calls, cost_usd }
    }

    fn toks(customer: Option<&str>, tokens: i64) -> TokensByDimension {
        TokensByDimension { key: customer.map(str::to_string), tokens }
    }

    fn window() -> (DateTime<Utc>, DateTime<Utc>) {
        (t("2026-06-01T00:00:00Z"), t("2026-07-01T00:00:00Z"))
    }

    #[test]
    fn assumptions_require_at_least_one_price() {
        let (s, u) = window();
        assert!(SimAssumptions::new(None, None, s, u).is_err());
        assert!(SimAssumptions::new(Some(2.0), None, s, u).is_ok());
        assert!(SimAssumptions::new(None, Some(9.0), s, u).is_ok());
    }

    #[test]
    fn window_days_drives_proration() {
        // A 30-day window prorates a flat fee at 1.0; a 15-day window at 0.5.
        let a = SimAssumptions::new(None, Some(30.0), t("2026-06-01T00:00:00Z"), t("2026-07-01T00:00:00Z"))
            .unwrap();
        assert!((a.window_days - 30.0).abs() < 1e-9);
        let b = SimAssumptions::new(None, Some(30.0), t("2026-06-01T00:00:00Z"), t("2026-06-16T00:00:00Z"))
            .unwrap();
        assert!((b.window_days - 15.0).abs() < 1e-9);
    }

    #[test]
    fn hypothetical_revenue_combines_token_charge_and_prorated_flat() {
        // 2M tokens @ $3/Mtok = $6, plus a $30 flat prorated to half a month = $15 → $21.
        let r = hypothetical_revenue(2_000_000, 3.0, 30.0, 0.5);
        assert!((r - 21.0).abs() < 1e-9);
        // Token-only and flat-only reduce cleanly.
        assert!((hypothetical_revenue(500_000, 4.0, 0.0, 1.0) - 2.0).abs() < 1e-9);
        assert!((hypothetical_revenue(0, 0.0, 30.0, 1.0) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn simulated_revenue_and_delta_vs_actual() {
        let (s, u) = window(); // 30-day window → proration 1.0
        // acme: real revenue $20, cost $1, 1M tokens. Simulate $5/Mtok + $10 flat → $15 revenue.
        let a = SimAssumptions::new(Some(5.0), Some(10.0), s, u).unwrap();
        let rows = compute_margin_simulation(
            &[rev("acme", 20.0, RevenueKind::OneTime, "2026-06-10T00:00:00Z")],
            &[cost(Some("acme"), 1.0, 3)],
            &[toks(Some("acme"), 1_000_000)],
            a,
            MarginDimension::Customer,
            s,
            u,
        );
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.key, "acme");
        assert_eq!(r.tokens, 1_000_000);
        assert!((r.actual_revenue_usd - 20.0).abs() < 1e-9);
        assert!((r.actual_margin_usd - 19.0).abs() < 1e-9);
        // 1M/1e6 * $5 = $5, + $10 flat * 1.0 = $15 simulated revenue; margin $15 − $1 = $14.
        assert!((r.simulated_revenue_usd - 15.0).abs() < 1e-9);
        assert!((r.simulated_margin_usd - 14.0).abs() < 1e-9);
        // Uplift = 14 − 19 = −5 (the hypothetical model earns less here).
        assert!((r.margin_delta_usd + 5.0).abs() < 1e-9);
    }

    #[test]
    fn free_tier_key_gets_simulated_revenue_from_its_tokens() {
        let (s, u) = window();
        // A free-tier customer: real revenue 0, cost $2, 4M tokens. Under $1/Mtok → $4 revenue.
        let a = SimAssumptions::new(Some(1.0), None, s, u).unwrap();
        let rows = compute_margin_simulation(
            &[],
            &[cost(Some("trial"), 2.0, 30)],
            &[toks(Some("trial"), 4_000_000)],
            a,
            MarginDimension::Customer,
            s,
            u,
        );
        let r = &rows[0];
        assert!((r.actual_margin_usd + 2.0).abs() < 1e-9, "was losing $2");
        assert!((r.simulated_revenue_usd - 4.0).abs() < 1e-9);
        assert!((r.simulated_margin_usd - 2.0).abs() < 1e-9, "now +$2");
        assert!((r.margin_delta_usd - 4.0).abs() < 1e-9, "uplift of $4");
    }

    #[test]
    fn rows_sorted_by_simulated_margin_ascending() {
        let (s, u) = window();
        let a = SimAssumptions::new(Some(2.0), None, s, u).unwrap();
        let rows = compute_margin_simulation(
            &[],
            &[cost(Some("light"), 0.5, 5), cost(Some("heavy"), 90.0, 900)],
            &[toks(Some("light"), 1_000_000), toks(Some("heavy"), 1_000_000)],
            a,
            MarginDimension::Customer,
            s,
            u,
        );
        // heavy: rev $2 − cost $90 = −$88; light: rev $2 − cost $0.5 = $1.5. Loser first.
        assert_eq!(rows[0].key, "heavy");
        assert!(rows[0].simulated_margin_usd < rows[1].simulated_margin_usd);
    }
}
