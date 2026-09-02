//! Resolving a rule's [`Threshold`] against the store, at the moment it is enforced.
//!
//! A [`Threshold::Fixed`] needs nothing — it is the number. A [`Threshold::RevenueShare`] needs the
//! recognized revenue for the rule's subject over the rule's window, which only the store can
//! answer, and which must be measured at *evaluation* time or the whole point (a cap that follows
//! the invoice) is lost.
//!
//! Two things this module is careful about:
//!
//! - **It costs nothing when nobody uses it.** [`needs_revenue`] is the gate every backend calls
//!   first, so a deployment with no revenue-share rule pays not one extra query on the ingest path.
//! - **It never invents a cap.** A backend that cannot serve revenue at all, or a window with no
//!   revenue rows, yields `None` → `+inf` → an inert rule that says so in its
//!   [`ThresholdBasis`](lighttrack_core::ThresholdBasis). The alternative — falling back to some
//!   default number — would turn an unmeasurable guardrail into a surprise 429.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use lighttrack_core::{
    recognized_revenue, LimitRule, LimitScope, RevenueEvent, Threshold, ThresholdBasis,
};

use crate::Result;

/// Whether this rule's threshold can only be known by reading revenue. The cheap pre-check every
/// admission path runs before it considers a store round-trip.
pub fn needs_revenue(rule: &LimitRule) -> bool {
    matches!(rule.threshold, Threshold::RevenueShare { .. })
}

/// The customer a revenue-share rule reads revenue for: the value of a `customer`-scoped rule, or
/// `None` for a rule that is not customer-scoped (project-wide revenue).
pub fn revenue_subject(rule: &LimitRule) -> Option<&str> {
    match rule.scope.as_ref() {
        Some(LimitScope::Customer(c)) => Some(c.as_str()),
        _ => None,
    }
}

/// Resolve every rule in `rules` that needs revenue, given a fetcher for the project's revenue rows
/// over one window. The fetcher is called at most **once per distinct window** across the whole rule
/// set, not once per rule — a project with a revenue-share rule per customer must not turn one
/// ingest into twenty scans.
///
/// Returns a map from rule id to the resolved `(threshold, basis)` pair; rules with a fixed
/// threshold are absent from it (their resolution needs nothing).
pub fn resolve_all<F>(
    rules: &[LimitRule],
    now: DateTime<Utc>,
    mut fetch: F,
) -> Result<HashMap<String, (f64, ThresholdBasis)>>
where
    F: FnMut(DateTime<Utc>, DateTime<Utc>) -> Result<Vec<RevenueEvent>>,
{
    let mut windows: RevenueWindows = HashMap::new();
    for r in rules.iter().filter(|r| needs_revenue(r)) {
        let key = window_key(r);
        if let std::collections::hash_map::Entry::Vacant(e) = windows.entry(key) {
            e.insert(fetch(r.window.since(now), now)?);
        }
    }
    Ok(resolve_from_windows(rules, now, &windows))
}

/// Revenue rows already read, keyed by window length in seconds — what an async backend prefetches
/// inside its own transaction before handing the pure resolution back here.
pub type RevenueWindows = HashMap<i64, Vec<RevenueEvent>>;

/// The key `windows` is indexed by for a given rule.
pub fn window_key(rule: &LimitRule) -> i64 {
    rule.window.lookback().num_seconds()
}

/// The pure half of [`resolve_all`]: resolve every revenue-share rule against revenue rows the
/// caller has already read. Split out so a backend whose reads are async (Postgres) can prefetch in
/// its own transaction and still share exactly this arithmetic — two implementations of "80% of
/// revenue" is precisely how a cap and its status come to disagree.
pub fn resolve_from_windows(
    rules: &[LimitRule],
    now: DateTime<Utc>,
    windows: &RevenueWindows,
) -> HashMap<String, (f64, ThresholdBasis)> {
    let mut out = HashMap::new();
    for r in rules.iter().filter(|r| needs_revenue(r)) {
        let Some(rows) = windows.get(&window_key(r)) else {
            continue;
        };
        let revenue = recognized_revenue(rows, revenue_subject(r), r.window.since(now), now);
        // No revenue rows at all for the subject is "we cannot measure this", not "$0.00": a
        // customer who has not been invoiced yet must not be capped at zero and hard-stopped.
        let measured = (!rows.is_empty() && revenue > 0.0).then_some(revenue);
        out.insert(r.id.clone(), r.threshold.resolve(measured));
    }
    out
}

/// The resolver `evaluate_admission` and the status surface take: hand it a map from [`resolve_all`]
/// (or an empty one) and it answers for every rule, falling back to the rule's own unmeasured
/// resolution.
pub fn resolver(
    resolved: &HashMap<String, (f64, ThresholdBasis)>,
) -> impl Fn(&LimitRule) -> (f64, ThresholdBasis) + '_ {
    move |rule: &LimitRule| {
        resolved
            .get(&rule.id)
            .copied()
            .unwrap_or_else(|| rule.threshold.resolve(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use lighttrack_core::{
        LimitAction, LimitMetric, LimitWindow, RevenueKind, ThresholdDimension, ThresholdKind,
    };

    fn rev(customer: &str, amount: f64, ts: DateTime<Utc>) -> RevenueEvent {
        RevenueEvent {
            id: "rev".into(),
            project_id: "p".into(),
            source: "manual".into(),
            external_id: None,
            customer_id: Some(customer.into()),
            product_id: None,
            amount_usd: amount,
            currency: "USD".into(),
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts,
            amount_minor: None,
            fx_rate: None,
            fx_book_version: None,
            converted: None,
        }
    }

    fn share_rule(id: &str, customer: Option<&str>) -> LimitRule {
        LimitRule {
            id: id.into(),
            project_id: "p".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Month,
            threshold: Threshold::RevenueShare {
                pct: 80.0,
                dimension: ThresholdDimension::Customer,
            },
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: customer.map(|c| LimitScope::Customer(c.into())),
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        }
    }

    #[test]
    fn a_fixed_rule_never_touches_the_store() {
        let mut r = share_rule("r", None);
        r.threshold = Threshold::Fixed(5.0);
        let mut calls = 0;
        let out = resolve_all(&[r], Utc::now(), |_, _| {
            calls += 1;
            Ok(vec![])
        })
        .unwrap();
        assert!(out.is_empty());
        assert_eq!(calls, 0, "a fixed cap must cost nothing on the hot path");
    }

    #[test]
    fn one_fetch_serves_every_rule_on_the_same_window() {
        let now = Utc::now();
        let rules = vec![
            share_rule("r1", Some("cus-a")),
            share_rule("r2", Some("cus-b")),
        ];
        let rows = vec![
            rev("cus-a", 412.0, now - Duration::days(1)),
            rev("cus-b", 100.0, now - Duration::days(1)),
        ];
        let mut calls = 0;
        let out = resolve_all(&rules, now, |_, _| {
            calls += 1;
            Ok(rows.clone())
        })
        .unwrap();
        assert_eq!(calls, 1, "one window, one scan");
        assert!((out["r1"].0 - 329.6).abs() < 1e-9);
        assert!((out["r2"].0 - 80.0).abs() < 1e-9);
        assert_eq!(out["r1"].1.kind, ThresholdKind::RevenueShare);
    }

    /// The invariant that keeps this feature from being dangerous: an unbilled customer is
    /// unmeasurable, not free. Capping them at `$0.00` would hard-stop them on their first call.
    #[test]
    fn a_subject_with_no_revenue_resolves_to_inert_not_to_zero() {
        let now = Utc::now();
        let rules = vec![share_rule("r1", Some("cus-new"))];
        let rows = vec![rev("cus-a", 412.0, now - Duration::days(1))];
        let out = resolve_all(&rules, now, |_, _| Ok(rows.clone())).unwrap();
        assert!(out["r1"].0.is_infinite());
        assert_eq!(out["r1"].1.kind, ThresholdKind::Unknown);
    }

    #[test]
    fn the_resolver_falls_back_to_the_rules_own_unmeasured_answer() {
        let mut fixed = share_rule("r", None);
        fixed.threshold = Threshold::Fixed(7.0);
        let empty = HashMap::new();
        let f = resolver(&empty);
        assert_eq!(f(&fixed).0, 7.0);
        assert!(f(&share_rule("other", None)).0.is_infinite());
    }
}
