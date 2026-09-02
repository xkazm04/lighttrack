//! The legacy grouped-rollup methods, expressed once over [`Store::rollup`].
//!
//! Nine `Store` methods asked one question in nine shapes, and four of them existed on SQLite only —
//! so the production Postgres deployment answered 501 for `/v1/forecast` and three margin surfaces.
//! These adapters make the nine a **consequence** of the one: a backend that implements `rollup`
//! gets all of them, with identical grouping, windowing and time-key semantics by construction
//! rather than by nine people writing nine `GROUP BY`s the same way.
//!
//! Backends that already hand-wrote a method keep their version (SQLite does); these are what the
//! trait defaults call, and the conformance suite asserts the two agree row for row.

use chrono::{DateTime, Utc};

mod margin;

pub(crate) use margin::{
    cost_by_dimension, customer_cost_by_model, customer_cost_by_name, tokens_by_dimension,
};

use lighttrack_core::{Dimension, LimitScope, RollupQuery, RollupRow, TimeKey};

use crate::{
    CostRow, DailyDimCost, DailyUsage, Result, ScopeUsage, Store, StoreError, Usage, UseCaseCostRow,
};

/// The lower bound for the legacy methods whose `since` is optional. No event predates it, and it
/// keeps the fixed-width RFC3339 string comparison well-formed (a `MIN_UTC` sentinel would not).
fn beginning() -> DateTime<Utc> {
    DateTime::UNIX_EPOCH
}

/// The billing-dimension map the legacy `dim: &str` arguments carry. Unknown values fall back to
/// `customer` — the behavior all three backends already shipped, kept identical here so migrating a
/// caller to `rollup` can't change what an unrecognized `?by=` answers with.
pub(super) fn legacy_dim(dim: &str) -> Dimension {
    match Dimension::parse(dim) {
        Some(Dimension::Product) => Dimension::Product,
        Some(Dimension::Prompt) => Dimension::Prompt,
        _ => Dimension::Customer,
    }
}

/// Re-label a backend's "I don't implement `rollup`" refusal as the capability the *caller* asked
/// for, so a 501 still names the surface an operator is missing.
pub(super) fn refusal<T>(r: Result<T>, what: &'static str) -> Result<T> {
    match r {
        Err(StoreError::Unsupported(_)) => Err(StoreError::Unsupported(what)),
        other => other,
    }
}

fn sort_by_cost_desc(rows: &mut [RollupRow]) {
    rows.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
}

pub(crate) fn cost_summary_windowed<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Vec<CostRow>> {
    let q = RollupQuery::new(
        &[Dimension::Project, Dimension::Provider, Dimension::Model],
        since.unwrap_or_else(beginning),
    )
    .project(project)
    .until(until);
    let mut rows = refusal(store.rollup(&q), "the windowed cost rollup")?;
    sort_by_cost_desc(&mut rows);
    Ok(rows
        .into_iter()
        .map(|r| CostRow {
            project_id: r.key(0).unwrap_or_default().to_string(),
            provider: r.key(1).unwrap_or_default().to_string(),
            model: r.key(2).unwrap_or_default().to_string(),
            calls: r.calls as i64,
            input_tokens: r.input_tokens as i64,
            output_tokens: r.output_tokens as i64,
            cost_usd: r.cost_usd,
            unpriced_calls: r.unpriced_calls as i64,
        })
        .collect())
}

pub(crate) fn usecase_costs<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<UseCaseCostRow>> {
    let q = RollupQuery::new(
        &[Dimension::Name, Dimension::Provider, Dimension::Model],
        since.unwrap_or_else(beginning),
    )
    .project(project);
    let mut rows = refusal(store.rollup(&q), "the use-case cost rollup")?;
    sort_by_cost_desc(&mut rows);
    Ok(rows
        .into_iter()
        .map(|r| UseCaseCostRow {
            name: r.key(0).map(str::to_string),
            provider: r.key(1).unwrap_or_default().to_string(),
            model: r.key(2).unwrap_or_default().to_string(),
            calls: r.calls as i64,
            input_tokens: r.input_tokens as i64,
            output_tokens: r.output_tokens as i64,
            cost_usd: r.cost_usd,
            unpriced_calls: r.unpriced_calls as i64,
        })
        .collect())
}

/// Rolling usage grouped by every value of one scope dimension. Keyed on **`received_at`**: this is
/// the pre-breach view of the same window admission enforces, and the two must measure the same
/// traffic or the page an operator reads before writing a cap describes different spend than the cap.
pub(crate) fn usage_by_scope<S: Store + ?Sized>(
    store: &S,
    project: &str,
    since: DateTime<Utc>,
    kind: &str,
) -> Result<Vec<ScopeUsage>> {
    if !LimitScope::KINDS.contains(&kind) {
        return Err(StoreError::Other(format!(
            "unknown scope dimension '{kind}'"
        )));
    }
    let dim = Dimension::parse(kind)
        .ok_or_else(|| StoreError::Other(format!("unknown scope dimension '{kind}'")))?;
    let q = RollupQuery::new(&[dim], since)
        .project(Some(project))
        .time_key(TimeKey::ReceivedAt);
    let mut rows = refusal(store.rollup(&q), "per-dimension usage breakdown")?;
    sort_by_cost_desc(&mut rows);
    Ok(rows
        .into_iter()
        .map(|r| ScopeUsage {
            value: r.key(0).map(str::to_string),
            usage: usage_of(&r),
        })
        .collect())
}

fn usage_of(r: &RollupRow) -> Usage {
    Usage {
        cost_usd: r.cost_usd,
        calls: r.calls as i64,
        tokens: r.tokens() as i64,
        unpriced_calls: r.unpriced_calls as i64,
        client_cost_usd: r.client_reported_cost_usd,
    }
}

pub(crate) fn daily_usage<S: Store + ?Sized>(
    store: &S,
    project: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<DailyUsage>> {
    let q = RollupQuery::new(&[Dimension::Day], since)
        .project(Some(project))
        .until(Some(until))
        .time_key(TimeKey::ReceivedAt);
    let mut rows = refusal(store.rollup(&q), "the daily usage series")?;
    rows.sort_by(|a, b| a.key(0).cmp(&b.key(0)));
    Ok(rows
        .into_iter()
        .map(|r| DailyUsage {
            day: r.key(0).unwrap_or_default().to_string(),
            cost_usd: r.cost_usd,
            calls: r.calls as i64,
            tokens: r.tokens() as i64,
        })
        .collect())
}

pub(crate) fn daily_cost_by_dimension<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<DailyDimCost>> {
    let q = RollupQuery::new(&[Dimension::Day, legacy_dim(dim)], since)
        .project(project)
        .until(Some(until))
        .time_key(TimeKey::ReceivedAt);
    let mut rows = refusal(store.rollup(&q), "the daily cost series")?;
    rows.sort_by(|a, b| a.key(0).cmp(&b.key(0)));
    Ok(rows
        .into_iter()
        .map(|r| DailyDimCost {
            day: r.key(0).unwrap_or_default().to_string(),
            key: r.key(1).map(str::to_string),
            cost_usd: r.cost_usd,
            calls: r.calls as i64,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `dim` strings the margin/forecast routes carry, mapped to dimensions. The lenient
    /// fallback is deliberate and pre-existing — pinned here so a rewrite can't quietly make an
    /// unknown `?by=` an error on one backend and customer data on another.
    #[test]
    fn the_legacy_dim_map_matches_what_the_backends_shipped() {
        assert_eq!(legacy_dim("product"), Dimension::Product);
        assert_eq!(legacy_dim("prompt"), Dimension::Prompt);
        assert_eq!(legacy_dim("customer"), Dimension::Customer);
        assert_eq!(legacy_dim(""), Dimension::Customer);
        assert_eq!(legacy_dim("nonsense"), Dimension::Customer);
        assert_eq!(legacy_dim("'; DROP TABLE events; --"), Dimension::Customer);
        // `day` is not a billing dimension — it must not become one by falling through.
        assert_eq!(legacy_dim("day"), Dimension::Customer);
    }
}
