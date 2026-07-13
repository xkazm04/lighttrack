//! Per-day margin trend — "is customer X's margin improving?" over a window.
//!
//! Revenue is recognized **per UTC day** by the *same* rules [`crate::compute_margin`] uses: this
//! calls the shared [`crate::margin::recognized_amount`] over each one-day sub-window, so a
//! subscription's daily slice, a point-in-time charge, and a refund all land exactly as they do in the
//! single-window rollup — no duplicated recognition math. Cost per day comes from the store's
//! per-dimension daily series ([`DailyKeyCost`], the core-native form the caller maps from
//! `DailyDimCost`). Keys are capped to the top-N by absolute total margin.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Duration, NaiveDate};
use serde::Serialize;

use crate::margin::{dim_key, recognized_amount, MarginDimension};
use crate::revenue::RevenueEvent;

/// One `(dimension key, UTC day)` LLM-cost input — the core-native form of the store's `DailyDimCost`,
/// mapped by the caller so `core` stays store-agnostic. `key` is the customer/product id (or the
/// caller's `unattributed` label); `day` is `YYYY-MM-DD`.
pub struct DailyKeyCost {
    pub day: String,
    pub key: String,
    pub cost_usd: f64,
}

/// One day's revenue / cost / margin for a single dimension key.
#[derive(Debug, Clone, Serialize)]
pub struct MarginTrendPoint {
    pub date: String,
    pub revenue_usd: f64,
    pub cost_usd: f64,
    pub margin_usd: f64,
}

/// A dense daily series for one key (or the synthetic `__total__`), plus its window totals.
#[derive(Debug, Clone, Serialize)]
pub struct MarginTrendSeries {
    pub key: String,
    pub points: Vec<MarginTrendPoint>,
    pub total_revenue_usd: f64,
    pub total_cost_usd: f64,
    pub total_margin_usd: f64,
}

/// The trend result: the top-N keys' series (by |margin|), an all-keys `totals` series, and the
/// pre-cap key count so the caller can say "showing 20 of N".
#[derive(Debug, Clone, Serialize)]
pub struct MarginTrend {
    pub series: Vec<MarginTrendSeries>,
    pub totals: MarginTrendSeries,
    pub key_count: usize,
    pub top_n: usize,
}

/// Compute the per-day margin trend over `days` UTC days starting at `start_day`. Revenue is recognized
/// per day via [`recognized_amount`]; cost is summed from `daily_cost` (days outside the window are
/// ignored). Series are dense (missing days are zero) and capped to `top_n` keys by |total margin|.
pub fn compute_margin_trend(
    revenue: &[RevenueEvent],
    daily_cost: &[DailyKeyCost],
    dim: MarginDimension,
    start_day: NaiveDate,
    days: u32,
    top_n: usize,
) -> MarginTrend {
    let day_strings: Vec<String> = (0..days)
        .map(|i| (start_day + Duration::days(i as i64)).format("%Y-%m-%d").to_string())
        .collect();

    // revenue[key][day] — recognize each event over each one-day sub-window (identical rules to the
    // single-window rollup). Zero-recognition days are simply not inserted.
    let mut rev: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for r in revenue {
        let key = dim_key(r, dim);
        for (i, day) in day_strings.iter().enumerate() {
            let d0 = (start_day + Duration::days(i as i64))
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc();
            let d1 = d0 + Duration::days(1);
            let amt = recognized_amount(r, d0, d1);
            if amt != 0.0 {
                *rev.entry(key.clone()).or_default().entry(day.clone()).or_default() += amt;
            }
        }
    }

    // cost[key][day] — only days inside the window (the store query may spill a boundary day).
    let in_range: BTreeSet<&str> = day_strings.iter().map(String::as_str).collect();
    let mut cost: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for c in daily_cost {
        if in_range.contains(c.day.as_str()) {
            *cost.entry(c.key.clone()).or_default().entry(c.day.clone()).or_default() += c.cost_usd;
        }
    }

    let keys: BTreeSet<String> = rev.keys().chain(cost.keys()).cloned().collect();
    let mut series: Vec<MarginTrendSeries> = keys
        .iter()
        .map(|k| build_series(k.clone(), &day_strings, rev.get(k), cost.get(k)))
        .collect();
    let key_count = series.len();

    // Totals across *all* keys, computed before the top-N cap so the totals row is complete.
    let totals = build_totals(&day_strings, &series);

    // Cap to the biggest movers by absolute margin (a large loss is as interesting as a large gain).
    series.sort_by(|a, b| b.total_margin_usd.abs().total_cmp(&a.total_margin_usd.abs()));
    series.truncate(top_n);

    MarginTrend { series, totals, key_count, top_n }
}

fn build_series(
    key: String,
    days: &[String],
    rev: Option<&BTreeMap<String, f64>>,
    cost: Option<&BTreeMap<String, f64>>,
) -> MarginTrendSeries {
    let mut points = Vec::with_capacity(days.len());
    let (mut tr, mut tc) = (0.0, 0.0);
    for d in days {
        let r = rev.and_then(|m| m.get(d)).copied().unwrap_or(0.0);
        let c = cost.and_then(|m| m.get(d)).copied().unwrap_or(0.0);
        tr += r;
        tc += c;
        points.push(MarginTrendPoint {
            date: d.clone(),
            revenue_usd: round(r),
            cost_usd: round(c),
            margin_usd: round(r - c),
        });
    }
    MarginTrendSeries {
        key,
        points,
        total_revenue_usd: round(tr),
        total_cost_usd: round(tc),
        total_margin_usd: round(tr - tc),
    }
}

fn build_totals(days: &[String], series: &[MarginTrendSeries]) -> MarginTrendSeries {
    let mut points = Vec::with_capacity(days.len());
    for (i, d) in days.iter().enumerate() {
        let r: f64 = series.iter().map(|s| s.points[i].revenue_usd).sum();
        let c: f64 = series.iter().map(|s| s.points[i].cost_usd).sum();
        points.push(MarginTrendPoint {
            date: d.clone(),
            revenue_usd: round(r),
            cost_usd: round(c),
            margin_usd: round(r - c),
        });
    }
    let tr: f64 = points.iter().map(|p| p.revenue_usd).sum();
    let tc: f64 = points.iter().map(|p| p.cost_usd).sum();
    MarginTrendSeries {
        key: "__total__".to_string(),
        points,
        total_revenue_usd: round(tr),
        total_cost_usd: round(tc),
        total_margin_usd: round(tr - tc),
    }
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::revenue::RevenueKind;
    use chrono::{DateTime, Utc};

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
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

    fn cost(key: &str, day: &str, cost_usd: f64) -> DailyKeyCost {
        DailyKeyCost { day: day.into(), key: key.into(), cost_usd }
    }

    #[test]
    fn point_in_time_lands_on_its_day_only() {
        let r = rev("acme", 30.0, RevenueKind::OneTime, "2026-06-02T12:00:00Z");
        let tr = compute_margin_trend(
            &[r],
            &[cost("acme", "2026-06-01", 1.0), cost("acme", "2026-06-02", 2.0)],
            MarginDimension::Customer,
            day("2026-06-01"),
            3,
            20,
        );
        let s = &tr.series[0];
        assert_eq!(s.key, "acme");
        assert_eq!(s.points.len(), 3);
        assert!((s.points[0].revenue_usd - 0.0).abs() < 1e-9, "day 1 no revenue");
        assert!((s.points[1].revenue_usd - 30.0).abs() < 1e-9, "all $30 on day 2");
        assert!((s.points[2].revenue_usd - 0.0).abs() < 1e-9, "day 3 no revenue");
        // Cost lands per day; margin = revenue − cost.
        assert!((s.points[0].margin_usd + 1.0).abs() < 1e-9);
        assert!((s.points[1].margin_usd - 28.0).abs() < 1e-9);
        assert!((s.total_revenue_usd - 30.0).abs() < 1e-9);
        assert!((s.total_cost_usd - 3.0).abs() < 1e-9);
        assert!((s.total_margin_usd - 27.0).abs() < 1e-9);
    }

    #[test]
    fn subscription_amortizes_evenly_across_days() {
        // $40 covering exactly the 4-day window → $10 recognized each day.
        let mut r = rev("acme", 40.0, RevenueKind::Subscription, "2026-06-01T00:00:00Z");
        r.period_start = Some(t("2026-06-01T00:00:00Z"));
        r.period_end = Some(t("2026-06-05T00:00:00Z"));
        let tr = compute_margin_trend(&[r], &[], MarginDimension::Customer, day("2026-06-01"), 4, 20);
        let s = &tr.series[0];
        for p in &s.points {
            assert!((p.revenue_usd - 10.0).abs() < 1e-6, "day {} got {}", p.date, p.revenue_usd);
        }
        assert!((s.total_revenue_usd - 40.0).abs() < 1e-6);
    }

    #[test]
    fn refund_is_negative_on_its_day() {
        let charge = rev("acme", 20.0, RevenueKind::OneTime, "2026-06-01T06:00:00Z");
        let refund = rev("acme", 5.0, RevenueKind::Refund, "2026-06-03T06:00:00Z");
        let tr = compute_margin_trend(
            &[charge, refund],
            &[],
            MarginDimension::Customer,
            day("2026-06-01"),
            3,
            20,
        );
        let s = &tr.series[0];
        assert!((s.points[0].revenue_usd - 20.0).abs() < 1e-9);
        assert!((s.points[2].revenue_usd + 5.0).abs() < 1e-9, "refund subtracts on day 3");
        assert!((s.total_revenue_usd - 15.0).abs() < 1e-9, "net revenue after refund");
    }

    #[test]
    fn top_n_caps_by_absolute_margin_and_totals_are_complete() {
        // Three customers; cap to 2. Totals must still reflect all three.
        let revenue = [
            rev("small", 1.0, RevenueKind::OneTime, "2026-06-01T00:00:00Z"),
            rev("winner", 100.0, RevenueKind::OneTime, "2026-06-01T00:00:00Z"),
        ];
        let costs = [
            cost("small", "2026-06-01", 0.5),
            cost("loser", "2026-06-01", 80.0), // big negative margin, no revenue
        ];
        let tr = compute_margin_trend(&revenue, &costs, MarginDimension::Customer, day("2026-06-01"), 1, 2);
        assert_eq!(tr.key_count, 3, "small + winner + loser");
        assert_eq!(tr.series.len(), 2, "capped to top-2");
        let keys: Vec<&str> = tr.series.iter().map(|s| s.key.as_str()).collect();
        assert!(keys.contains(&"winner"), "biggest positive margin kept");
        assert!(keys.contains(&"loser"), "biggest negative margin kept");
        assert!(!keys.contains(&"small"), "smallest |margin| dropped");
        // Totals cover all three: revenue 101, cost 80.5, margin 20.5.
        assert!((tr.totals.total_revenue_usd - 101.0).abs() < 1e-9);
        assert!((tr.totals.total_cost_usd - 80.5).abs() < 1e-9);
        assert!((tr.totals.total_margin_usd - 20.5).abs() < 1e-9);
    }
}
