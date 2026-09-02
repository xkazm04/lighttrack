//! The margin/what-if half of the compat layer: cost and tokens per billing dimension, and one
//! customer's spend split by model or use-case.
//!
//! These four are the ones that were SQLite-only. `/v1/margin/simulate`, `/v1/margin/trend` and
//! `/v1/margin/customer/:id` answered 501 on the production Postgres backend purely because nobody
//! had written the same `GROUP BY` a fourth and fifth time.

use chrono::{DateTime, Utc};

use lighttrack_core::{CostByDimension, Dimension, RollupQuery, RollupRow, TokensByDimension};

use super::{legacy_dim, refusal};
use crate::{CustomerCostRow, Result, Store};

pub(crate) fn cost_by_dimension<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CostByDimension>> {
    let q = RollupQuery::new(&[legacy_dim(dim)], since)
        .project(project)
        .until(Some(until));
    let rows = refusal(store.rollup(&q), "cost by dimension")?;
    Ok(rows
        .into_iter()
        .map(|r| CostByDimension {
            key: r.key(0).map(str::to_string),
            calls: r.calls as i64,
            cost_usd: r.cost_usd,
            unpriced_calls: r.unpriced_calls as i64,
        })
        .collect())
}

pub(crate) fn tokens_by_dimension<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<TokensByDimension>> {
    let q = RollupQuery::new(&[legacy_dim(dim)], since)
        .project(project)
        .until(Some(until));
    let rows = refusal(store.rollup(&q), "token usage by dimension")?;
    Ok(rows
        .into_iter()
        .map(|r| TokensByDimension {
            key: r.key(0).map(str::to_string),
            tokens: r.tokens() as i64,
        })
        .collect())
}

/// One customer's cost split by `provider/model` or by use-case `name`. The customer is a *filter*,
/// not a grouping — a row for anyone else in this answer is a tenant leak, which is why the filter
/// rides the same `Dimension` the margin rollup groups on.
fn customer_cost<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    customer: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    group_by: &[Dimension],
    key: impl Fn(&RollupRow) -> String,
) -> Result<Vec<CustomerCostRow>> {
    let q = RollupQuery::new(group_by, since)
        .project(project)
        .until(Some(until))
        .filter(Dimension::Customer, customer);
    let rows = refusal(store.rollup(&q), "customer cost breakdown")?;
    let mut out: Vec<CustomerCostRow> = rows
        .iter()
        .map(|r| CustomerCostRow {
            key: key(r),
            calls: r.calls as i64,
            cost_usd: r.cost_usd,
        })
        .collect();
    out.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd).then(a.key.cmp(&b.key)));
    Ok(out)
}

pub(crate) fn customer_cost_by_model<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    customer: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CustomerCostRow>> {
    customer_cost(
        store,
        project,
        customer,
        since,
        until,
        &[Dimension::Provider, Dimension::Model],
        |r| {
            format!(
                "{}/{}",
                r.key(0).unwrap_or_default(),
                r.key(1).unwrap_or_default()
            )
        },
    )
}

pub(crate) fn customer_cost_by_name<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    customer: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CustomerCostRow>> {
    customer_cost(
        store,
        project,
        customer,
        since,
        until,
        &[Dimension::Name],
        |r| r.key(0).unwrap_or("(unnamed)").to_string(),
    )
}
