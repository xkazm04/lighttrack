//! One authority for "how much has this scope spent" when the scope's membership is wider than
//! SQL equality.
//!
//! [`LimitScope::matches`] decides a `Model` rule *applies* by canonical family: a `gpt-4o` cap
//! catches `gpt-4o-2024-08-06` and `openai/gpt-4o`, because a dated release is the same model for
//! spending purposes. Every backend's `usage_since_scoped` computed the number that rule is
//! *evaluated against* with `WHERE model = ?`, so a cap over dated-snapshot traffic read
//! `current ≈ 0` forever: `ratio` never left the first percent, `warn_at` never fired, `breached`
//! was never true, and the rule still reported as configured and enforcing. The gate rejected
//! traffic the ledger refused to count.
//!
//! The fix is to stop writing membership twice. A `Model` scope's rolling usage is a fold of the
//! window grouped by `(provider, model)` — the existing [`RollupQuery`] primitive, keyed on
//! [`TimeKey::ReceivedAt`], the same clock `usage_since` measures on — through
//! [`LimitScope::matches`] itself. The exact-value dimensions keep their indexed equality query,
//! because for them exact *is* the matcher ([`LimitScope::folds_canonical_family`] is the test).
//!
//! A backend whose [`Store::rollup`](crate::Store::rollup) is unavailable propagates
//! [`StoreError::Unsupported`](crate::StoreError::Unsupported) → HTTP 501. **A short count is the
//! bug; a 501 is honest.**

use chrono::{DateTime, Utc};

use lighttrack_core::{Dimension, LimitScope, RollupQuery, RollupRow, TimeKey};

use crate::{Result, Usage};

/// The grouped query a scope fold reads. Exposed so each backend builds the identical window —
/// `received_at`, project-scoped, `[since, until)` — rather than three near-identical queries.
fn fold_query<'a>(
    project: &'a str,
    since: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
) -> RollupQuery<'a> {
    RollupQuery::new(&[Dimension::Provider, Dimension::Model], since)
        .project(Some(project))
        .until(until)
        .time_key(TimeKey::ReceivedAt)
}

/// Rolling usage for one `(window, scope)` where the scope's membership is the canonical-family
/// fold. `rollup` is the backend's own grouped-rollup entry point.
///
/// `until` is `None` for the live admission/status window (the rolling look-back has no upper
/// bound); it exists because a replay of a past instant needs one and the primitive already
/// carries it.
pub fn usage_for_scope<F>(
    rollup: F,
    project: &str,
    since: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    scope: &LimitScope,
) -> Result<Usage>
where
    F: FnOnce(&RollupQuery<'_>) -> Result<Vec<RollupRow>>,
{
    let rows = rollup(&fold_query(project, since, until))?;
    Ok(fold(&rows, scope))
}

/// Sum the rows whose `(provider, model)` falls under `scope`.
///
/// Carries `unpriced_calls` and `client_cost_usd` through, so [`Usage::cost_evidence`] stays intact
/// on a scoped read: a cap must still be able to impute a charge for traffic the price book does
/// not know, and must still refuse to enforce on a window it cannot price.
fn fold(rows: &[RollupRow], scope: &LimitScope) -> Usage {
    let mut u = Usage::default();
    for r in rows {
        let provider = r.key(0).unwrap_or("");
        let model = r.key(1).unwrap_or("");
        if !scope.matches_model_row(provider, model) {
            continue;
        }
        u.cost_usd += r.cost_usd;
        u.calls += r.calls as i64;
        u.tokens += r.tokens() as i64;
        u.unpriced_calls += r.unpriced_calls as i64;
        u.client_cost_usd += r.client_reported_cost_usd;
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, model: &str, calls: u64, cost: f64) -> RollupRow {
        RollupRow {
            keys: vec![Some(provider.into()), Some(model.into())],
            calls,
            input_tokens: calls * 10,
            output_tokens: calls * 5,
            cost_usd: cost,
            unpriced_calls: 0,
            client_reported_cost_usd: 0.0,
            errors: 0,
        }
    }

    #[test]
    fn a_model_scope_folds_the_canonical_family_and_nothing_else() {
        let rows = vec![
            row("openai", "gpt-4o", 1, 1.0),
            row("openai", "openai/gpt-4o", 1, 2.0),
            row("openai", "gpt-4o-2024-08-06", 1, 4.0),
            row("openai", "gpt-4o-mini", 1, 8.0),
        ];
        let u = fold(&rows, &LimitScope::Model("gpt-4o".into()));
        assert_eq!(u.calls, 3, "three spellings of one family");
        assert!((u.cost_usd - 7.0).abs() < 1e-9);
        assert_eq!(u.tokens, 45);

        let mini = fold(&rows, &LimitScope::Model("gpt-4o-mini".into()));
        assert_eq!(mini.calls, 1, "a different family is outside the scope");
        assert_eq!(
            u.calls + mini.calls,
            4,
            "the two families partition the rows"
        );
    }

    #[test]
    fn the_fold_carries_the_cost_evidence_a_cap_needs() {
        let mut r = row("openai", "gpt-4o-2024-08-06", 4, 3.0);
        r.unpriced_calls = 2;
        r.client_reported_cost_usd = 1.5;
        let u = fold(&[r], &LimitScope::Model("gpt-4o".into()));
        assert_eq!(u.unpriced_calls, 2, "unpriced traffic is still counted");
        assert!((u.client_cost_usd - 1.5).abs() < 1e-9);
    }

    #[test]
    fn only_the_model_dimension_is_wider_than_equality() {
        assert!(LimitScope::Model("gpt-4o".into()).folds_canonical_family());
        for s in [
            LimitScope::Provider("openai".into()),
            LimitScope::Name("summarize".into()),
            LimitScope::ApiKey("k1".into()),
            LimitScope::Customer("cus_1".into()),
        ] {
            assert!(
                !s.folds_canonical_family(),
                "{} keeps its indexed equality query",
                s.label()
            );
        }
    }

    #[test]
    fn the_fold_window_is_the_accounting_clock_on_both_ends() {
        let since = Utc::now() - chrono::Duration::hours(1);
        let until = Utc::now();
        let q = fold_query("p1", since, Some(until));
        assert_eq!(q.time_key, TimeKey::ReceivedAt, "backdating must not help");
        assert_eq!(q.project, Some("p1"));
        assert_eq!(q.group_by, vec![Dimension::Provider, Dimension::Model]);
        assert_eq!(q.until, Some(until));
        assert_eq!(q.invalid(), None, "the fold never builds a refused query");
    }
}
