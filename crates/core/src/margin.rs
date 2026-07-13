//! Profit/margin computation — the pure, I/O-free heart of cost/profit tracking.
//!
//! `margin = recognized revenue − attributed LLM cost`, rolled up per customer or per product over a
//! window. Cost comes from the events table (monitored ingest traffic only — the judge/benchmark
//! engine writes to `scores`, not `events`, so summing event cost is already COGS-correct and excludes
//! eval spend by construction). Revenue comes from [`crate::RevenueEvent`]s, recognized into the window
//! by amortizing subscriptions across their period and netting refunds.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::revenue::{RevenueEvent, RevenueKind};

/// Label used when cost or revenue carries no customer/product id.
pub const UNATTRIBUTED: &str = "unattributed";

/// Which axis to roll margin up by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginDimension {
    Customer,
    Product,
}

impl MarginDimension {
    pub fn parse(s: &str) -> Self {
        match s {
            "product" => MarginDimension::Product,
            _ => MarginDimension::Customer,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MarginDimension::Customer => "customer",
            MarginDimension::Product => "product",
        }
    }
}

/// LLM cost aggregated for one dimension value over the window (produced by the store from events).
#[derive(Debug, Clone)]
pub struct CostByDimension {
    /// The customer/product id, or `None` for untagged (unattributed) cost.
    pub key: Option<String>,
    pub calls: i64,
    pub cost_usd: f64,
}

/// One profit/margin rollup row.
#[derive(Debug, Clone, Serialize)]
pub struct MarginRow {
    /// Dimension value (customer/product id), or [`UNATTRIBUTED`].
    pub key: String,
    pub revenue_usd: f64,
    pub llm_cost_usd: f64,
    pub gross_margin_usd: f64,
    /// `gross_margin / revenue`; `None` when there's no revenue (cost-only / unattributed rows).
    pub margin_pct: Option<f64>,
    pub calls: i64,
}

/// Compute per-dimension margin over `[since, until)`. Revenue is recognized by amortizing each event
/// across the overlap of its `[period_start, period_end]` with the window; point-in-time revenue (no
/// period) counts fully when `ts ∈ [since, until)`. Refunds subtract. Rows are sorted by margin
/// ascending, so the most unprofitable dimension is first.
pub fn compute_margin(
    revenue: &[RevenueEvent],
    costs: &[CostByDimension],
    dim: MarginDimension,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<MarginRow> {
    let mut rev: BTreeMap<String, f64> = BTreeMap::new();
    for r in revenue {
        let recognized = recognized_amount(r, since, until);
        if recognized == 0.0 {
            continue;
        }
        *rev.entry(dim_key(r, dim)).or_default() += recognized;
    }

    let mut cost: BTreeMap<String, (f64, i64)> = BTreeMap::new();
    for c in costs {
        let key = c.key.clone().unwrap_or_else(|| UNATTRIBUTED.to_string());
        let e = cost.entry(key).or_insert((0.0, 0));
        e.0 += c.cost_usd;
        e.1 += c.calls;
    }

    let keys: BTreeSet<String> = rev.keys().chain(cost.keys()).cloned().collect();
    let mut rows: Vec<MarginRow> = keys
        .into_iter()
        .map(|k| {
            let revenue_usd = *rev.get(&k).unwrap_or(&0.0);
            let (llm_cost_usd, calls) = cost.get(&k).copied().unwrap_or((0.0, 0));
            let gross = revenue_usd - llm_cost_usd;
            MarginRow {
                key: k,
                revenue_usd: round(revenue_usd),
                llm_cost_usd: round(llm_cost_usd),
                gross_margin_usd: round(gross),
                margin_pct: (revenue_usd > 0.0).then(|| gross / revenue_usd),
                calls,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.gross_margin_usd.total_cmp(&b.gross_margin_usd));
    rows
}

pub(crate) fn dim_key(r: &RevenueEvent, dim: MarginDimension) -> String {
    match dim {
        MarginDimension::Customer => r.customer_id.clone(),
        MarginDimension::Product => r.product_id.clone(),
    }
    .unwrap_or_else(|| UNATTRIBUTED.to_string())
}

/// Amount of `r` recognized within `[since, until)`. Refunds are negative. Subscriptions with a valid
/// period amortize linearly across the overlap; everything else is recognized fully at `ts`. Shared by
/// the window rollup ([`compute_margin`]) and the per-day trend ([`crate::margin_trend`]) so both apply
/// identical recognition rules — a daily point is just this over a one-day window.
pub(crate) fn recognized_amount(r: &RevenueEvent, since: DateTime<Utc>, until: DateTime<Utc>) -> f64 {
    let signed = if r.kind == RevenueKind::Refund {
        -r.amount_usd.abs()
    } else {
        r.amount_usd.abs()
    };
    match (r.period_start, r.period_end) {
        (Some(ps), Some(pe)) if pe > ps => {
            let start = ps.max(since);
            let end = pe.min(until);
            if end <= start {
                return 0.0;
            }
            let overlap = (end - start).num_seconds() as f64;
            let total = (pe - ps).num_seconds() as f64;
            signed * (overlap / total)
        }
        _ => {
            if r.ts >= since && r.ts < until {
                signed
            } else {
                0.0
            }
        }
    }
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

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
        CostByDimension {
            key: customer.map(str::to_string),
            calls,
            cost_usd,
        }
    }

    fn window() -> (DateTime<Utc>, DateTime<Utc>) {
        (t("2026-06-01T00:00:00Z"), t("2026-07-01T00:00:00Z"))
    }

    #[test]
    fn healthy_margin() {
        let (s, u) = window();
        let rows = compute_margin(
            &[rev("acme", 20.0, RevenueKind::Subscription, "2026-06-10T00:00:00Z")],
            &[cost(Some("acme"), 0.87, 412)],
            MarginDimension::Customer,
            s,
            u,
        );
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.key, "acme");
        assert!((r.revenue_usd - 20.0).abs() < 1e-9);
        assert!((r.gross_margin_usd - 19.13).abs() < 1e-9);
        assert!((r.margin_pct.unwrap() - 0.9565).abs() < 1e-3);
        assert_eq!(r.calls, 412);
    }

    #[test]
    fn paying_but_unprofitable_is_surfaced_first() {
        let (s, u) = window();
        let rows = compute_margin(
            &[
                rev("acme", 20.0, RevenueKind::OneTime, "2026-06-10T00:00:00Z"),
                rev("heavy", 99.0, RevenueKind::OneTime, "2026-06-10T00:00:00Z"),
            ],
            &[cost(Some("acme"), 0.87, 10), cost(Some("heavy"), 142.5, 9000)],
            MarginDimension::Customer,
            s,
            u,
        );
        // sorted by margin ascending → the money-loser comes first
        assert_eq!(rows[0].key, "heavy");
        assert!((rows[0].gross_margin_usd + 43.5).abs() < 1e-9);
        assert!(rows[0].margin_pct.unwrap() < 0.0);
    }

    #[test]
    fn free_tier_is_negative_with_no_margin_pct() {
        let (s, u) = window();
        let rows = compute_margin(
            &[],
            &[cost(Some("trial-42"), 2.1, 30)],
            MarginDimension::Customer,
            s,
            u,
        );
        assert_eq!(rows[0].key, "trial-42");
        assert!((rows[0].gross_margin_usd + 2.1).abs() < 1e-9);
        assert!(rows[0].margin_pct.is_none()); // no revenue → undefined %
    }

    #[test]
    fn refund_reduces_recognized_revenue() {
        let (s, u) = window();
        let rows = compute_margin(
            &[
                rev("acme", 20.0, RevenueKind::OneTime, "2026-06-05T00:00:00Z"),
                rev("acme", 5.0, RevenueKind::Refund, "2026-06-20T00:00:00Z"),
            ],
            &[cost(Some("acme"), 1.0, 5)],
            MarginDimension::Customer,
            s,
            u,
        );
        assert!((rows[0].revenue_usd - 15.0).abs() < 1e-9);
        assert!((rows[0].gross_margin_usd - 14.0).abs() < 1e-9);
    }

    #[test]
    fn subscription_amortizes_across_window() {
        // $30 covering a 30-day period; only the 10 days inside the window are recognized → $10.
        let mut r = rev("acme", 30.0, RevenueKind::Subscription, "2026-05-21T00:00:00Z");
        r.period_start = Some(t("2026-05-21T00:00:00Z"));
        r.period_end = Some(t("2026-06-20T00:00:00Z"));
        let rows = compute_margin(
            &[r],
            &[],
            MarginDimension::Customer,
            t("2026-06-01T00:00:00Z"),
            t("2026-06-11T00:00:00Z"),
        );
        assert!((rows[0].revenue_usd - 10.0).abs() < 1e-6);
    }

    #[test]
    fn untagged_cost_lands_in_unattributed() {
        let (s, u) = window();
        let rows = compute_margin(&[], &[cost(None, 0.5, 3)], MarginDimension::Customer, s, u);
        assert_eq!(rows[0].key, UNATTRIBUTED);
        assert!(rows[0].margin_pct.is_none());
    }

    #[test]
    fn out_of_window_revenue_is_excluded() {
        let (s, u) = window();
        let rows = compute_margin(
            &[rev("acme", 20.0, RevenueKind::OneTime, "2026-05-01T00:00:00Z")],
            &[],
            MarginDimension::Customer,
            s,
            u,
        );
        assert!(rows.is_empty());
    }
}
