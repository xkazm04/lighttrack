//! The one grouped rollup, Postgres.
//!
//! This is what closes the parity gap the design named: `daily_usage`, `daily_cost_by_dimension`,
//! `tokens_by_dimension` and the two `customer_cost_by_*` breakdowns existed on SQLite only, so the
//! production Postgres deployment answered 501 for `/v1/forecast` and three `/v1/margin/*` routes
//! and the forecast sweep warned once per project per tick. Implementing this one function makes
//! all five answer, through the trait's default impls.
//!
//! Every interpolated fragment is a fixed literal chosen by a [`Dimension`] variant; values are
//! always bound. The `metadata` extraction carries the same `NULLIF(...,'')::jsonb` guard as the
//! rest of the crate — a bare cast **raises** on invalid JSON, which would fail the whole rollup
//! rather than skew one bucket (see the note in `events/cols.rs`).

use sqlx::postgres::PgPool;
use sqlx::Row;

use lighttrack_core::{Dimension, RollupQuery, RollupRow, Storage, TimeKey};
use lighttrack_store::{Result, StoreError};

use crate::util::{fmt_ts, pgerr};

/// The SQL expression yielding one dimension's value for a row, under `time` as the window key.
fn key_expr(d: Dimension, time: &str) -> String {
    match d.storage() {
        Storage::Column(c) => c.to_string(),
        Storage::MetadataKey(k) => format!("(NULLIF(metadata,'')::jsonb)->>'{k}'"),
        Storage::Day => format!("substr({time},1,10)"),
    }
}

fn time_expr(k: TimeKey) -> &'static str {
    match k {
        TimeKey::Ts => "ts",
        TimeKey::ReceivedAt => "COALESCE(received_at, ts)",
    }
}

/// The SQL and its binds, built without a database so the unit tests below can pin the shape.
fn build(q: &RollupQuery<'_>) -> std::result::Result<(String, Vec<String>), String> {
    if let Some(why) = q.invalid() {
        return Err(why);
    }
    let time = time_expr(q.time_key);
    let mut binds: Vec<String> = Vec::new();
    let mut conds: Vec<String> = Vec::new();

    if let Some(p) = q.project {
        binds.push(p.to_string());
        conds.push(format!("project_id = ${}", binds.len()));
    }
    binds.push(fmt_ts(q.since));
    conds.push(format!("{time} >= ${}", binds.len()));
    if let Some(u) = q.until {
        binds.push(fmt_ts(u));
        conds.push(format!("{time} < ${}", binds.len()));
    }
    if q.unpriced_only {
        conds.push("cost_usd IS NULL".to_string());
    }
    for (d, v) in &q.filter {
        binds.push(v.clone());
        conds.push(format!("{} = ${}", key_expr(*d, time), binds.len()));
    }

    let keys: Vec<String> = q.group_by.iter().map(|d| key_expr(*d, time)).collect();
    let select: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{e} AS k{i}"))
        .collect();
    // Group by the expressions, not by ordinal: `GROUP BY 1` is legal but a `jsonb` extraction
    // repeated in the projection and the grouping is what the `metadata_guard` test pins, so keep
    // the two textually identical.
    let sql = format!(
        "SELECT {sel}, COUNT(*)::bigint, COALESCE(SUM(input_tokens),0)::bigint, \
         COALESCE(SUM(output_tokens),0)::bigint, COALESCE(SUM(cost_usd),0.0), \
         COUNT(*) FILTER (WHERE cost_usd IS NULL)::bigint, \
         COALESCE(SUM(cost_usd) FILTER \
             (WHERE (NULLIF(metadata,'')::jsonb)->>'cost_source' = 'client'),0.0), \
         COUNT(*) FILTER (WHERE status <> 'success')::bigint \
         FROM events WHERE {conds} GROUP BY {group}",
        sel = select.join(", "),
        conds = conds.join(" AND "),
        group = keys.join(", "),
    );
    Ok((sql, binds))
}

pub(crate) async fn rollup(pool: &PgPool, q: &RollupQuery<'_>) -> Result<Vec<RollupRow>> {
    let (sql, binds) = build(q).map_err(StoreError::Other)?;
    let mut query = sqlx::query(&sql);
    for b in &binds {
        query = query.bind(b.clone());
    }
    let rows = query.fetch_all(pool).await.map_err(pgerr)?;
    let n = q.group_by.len();
    rows.iter()
        .map(|r| {
            let mut keys = Vec::with_capacity(n);
            for i in 0..n {
                keys.push(r.try_get::<Option<String>, _>(i).map_err(pgerr)?);
            }
            let count = |i: usize| -> Result<u64> {
                Ok(r.try_get::<i64, _>(i).map_err(pgerr)?.max(0) as u64)
            };
            Ok(RollupRow {
                keys,
                calls: count(n)?,
                input_tokens: count(n + 1)?,
                output_tokens: count(n + 2)?,
                cost_usd: r.try_get(n + 3).map_err(pgerr)?,
                unpriced_calls: count(n + 4)?,
                client_reported_cost_usd: r.try_get(n + 5).map_err(pgerr)?,
                errors: count(n + 6)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn q<'a>(dims: &[Dimension]) -> RollupQuery<'a> {
        RollupQuery::new(dims, Utc::now()).project(Some("p1"))
    }

    /// Values reach the statement as binds; a dimension only ever contributes a fixed literal. If
    /// this ever stopped holding, `?by=` and `?filter=` from the wire would be SQL injection.
    #[test]
    fn filter_values_are_bound_never_interpolated() {
        let query =
            q(&[Dimension::Customer]).filter(Dimension::Product, "'; DROP TABLE events; --");
        let (sql, binds) = build(&query).expect("valid");
        assert!(!sql.contains("DROP TABLE"), "{sql}");
        assert!(binds.iter().any(|b| b.contains("DROP TABLE")));
        assert!(sql.contains("->>'product_id' = $"), "{sql}");
    }

    /// `metadata::jsonb` raises on invalid JSON, so the guard must appear in **both** the projection
    /// and the `GROUP BY` — a `GROUP BY` expression is evaluated over every candidate row, so
    /// guarding only the projection fixes nothing. Same property `revenue.rs` pins for `cost_sql`.
    #[test]
    fn the_metadata_extraction_is_guarded_everywhere_it_appears() {
        let (sql, _) = build(&q(&[Dimension::Customer])).expect("valid");
        assert!(!sql.contains("(metadata::jsonb)"), "bare cast: {sql}");
        assert!(
            sql.matches("(NULLIF(metadata,'')::jsonb)->>'customer_id'")
                .count()
                >= 2,
            "projection and GROUP BY must both carry the guard: {sql}"
        );
    }

    #[test]
    fn the_window_key_is_selectable_and_day_buckets_ride_it() {
        let (ts, _) = build(&q(&[Dimension::Day])).expect("valid");
        assert!(ts.contains("substr(ts,1,10)"), "{ts}");
        let (recv, _) = build(&q(&[Dimension::Day]).time_key(TimeKey::ReceivedAt)).expect("valid");
        assert!(
            recv.contains("substr(COALESCE(received_at, ts),1,10)"),
            "accounting buckets on server arrival: {recv}"
        );
    }

    /// Placeholders are numbered in push order; a mismatch would bind the window to the project.
    #[test]
    fn placeholders_are_numbered_in_bind_order() {
        let query = q(&[Dimension::Model])
            .until(Some(Utc::now()))
            .filter(Dimension::Customer, "acme");
        let (sql, binds) = build(&query).expect("valid");
        assert_eq!(binds.len(), 4, "project, since, until, filter");
        assert!(sql.contains("project_id = $1"), "{sql}");
        assert!(sql.contains("ts >= $2") && sql.contains("ts < $3"), "{sql}");
        assert!(sql.contains("= $4"), "{sql}");
    }

    /// An all-projects rollup drops the project predicate entirely rather than binding a NULL —
    /// and every later placeholder shifts down with it.
    #[test]
    fn an_all_projects_rollup_numbers_from_the_window() {
        let (sql, binds) =
            build(&RollupQuery::new(&[Dimension::Model], Utc::now())).expect("valid");
        assert!(!sql.contains("project_id"), "{sql}");
        assert_eq!(binds.len(), 1);
        assert!(sql.contains("ts >= $1"), "{sql}");
    }

    /// The unpriced ledger's predicate narrows the rows the sums are taken over — not just the
    /// `unpriced_calls` disclosure column, whose token sums cover priced calls too.
    #[test]
    fn unpriced_only_adds_a_row_predicate_not_a_projection() {
        let (plain, _) = build(&q(&[Dimension::Model])).expect("valid");
        assert!(!plain.contains("WHERE project_id = $1 AND ts >= $2 AND cost_usd IS NULL"));
        let (only, binds) = build(&q(&[Dimension::Model]).only_unpriced()).expect("valid");
        assert!(only.contains("AND cost_usd IS NULL"), "{only}");
        assert_eq!(binds.len(), 2, "the predicate binds nothing");
    }

    #[test]
    fn a_malformed_query_never_becomes_sql() {
        assert!(build(&q(&[])).is_err());
        assert!(build(&q(&[Dimension::Model, Dimension::Model])).is_err());
    }
}
