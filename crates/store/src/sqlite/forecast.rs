//! Daily (UTC) usage/cost time-series for predictive forecasting.
//!
//! The day bucket is `substr(ts, 1, 10)` — the `YYYY-MM-DD` prefix of the fixed-width RFC3339
//! timestamp. Because every backend stores `ts` in UTC via [`crate::codec::fmt_ts`], that prefix is
//! the UTC calendar day, and grouping by it is correct without any date-math in SQL. Cost is summed
//! straight off `events`, so it's COGS-correct by construction (judge/benchmark spend lives in
//! `scores`, never `events`) — the same property the margin rollups rely on.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use crate::codec::fmt_ts;
use crate::{DailyDimCost, DailyUsage, Result};

/// Per-day project totals (cost, calls, tokens) over `[since, until)`, oldest day first.
pub(super) fn daily_usage(
    conn: &Connection,
    project: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<DailyUsage>> {
    let sql = "SELECT substr(ts,1,10) AS day, COALESCE(SUM(cost_usd),0.0) AS cost, \
               COUNT(*) AS calls, COALESCE(SUM(input_tokens + output_tokens),0) AS tokens \
               FROM events WHERE project_id = ?1 AND ts >= ?2 AND ts < ?3 \
               GROUP BY day ORDER BY day ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params![project, fmt_ts(since), fmt_ts(until)], |r: &Row| {
            Ok(DailyUsage {
                day: r.get(0)?,
                cost_usd: r.get(1)?,
                calls: r.get(2)?,
                tokens: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Per-day cost for each billing-dimension value (`customer` | `product`) over `[since, until)`,
/// oldest day first. Untagged calls group under a NULL key.
pub(super) fn daily_cost_by_dimension(
    conn: &Connection,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<DailyDimCost>> {
    let path = match dim {
        "product" => "$.product_id",
        _ => "$.customer_id",
    };
    let sql = format!(
        "SELECT substr(ts,1,10) AS day, json_extract(metadata, '{path}') AS k, \
         COALESCE(SUM(cost_usd),0.0) AS cost, COUNT(*) AS calls \
         FROM events WHERE {proj} AND ts >= ?2 AND ts < ?3 \
         GROUP BY day, k ORDER BY day ASC",
        proj = super::project_pred(project),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![project, fmt_ts(since), fmt_ts(until)], |r: &Row| {
            Ok(DailyDimCost {
                day: r.get(0)?,
                key: r.get::<_, Option<String>>(1)?,
                cost_usd: r.get(2)?,
                calls: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::parse_ts;
    use lighttrack_core::LlmEvent;
    use serde_json::json;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../../../schema/sqlite/001_init.sql"))
            .unwrap();
        c
    }

    fn ev(customer: &str, cost: f64, ts: &str) -> LlmEvent {
        serde_json::from_value(json!({
            "id": format!("e-{customer}-{ts}"), "project_id": "p1",
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "ts": ts, "cost_usd": cost, "usage": { "input": 10, "output": 5 },
            "metadata": { "customer_id": customer }
        }))
        .unwrap()
    }

    #[test]
    fn daily_usage_buckets_by_utc_day() {
        let c = conn();
        for e in [
            ev("acme", 0.50, "2026-06-10T01:00:00Z"),
            ev("acme", 0.30, "2026-06-10T22:00:00Z"), // same UTC day as above
            ev("acme", 1.20, "2026-06-11T05:00:00Z"),
        ] {
            super::super::events::insert(&c, &e).unwrap();
        }
        let since = parse_ts("2026-06-01T00:00:00Z").unwrap();
        let until = parse_ts("2026-07-01T00:00:00Z").unwrap();
        let series = daily_usage(&c, "p1", since, until).unwrap();
        assert_eq!(series.len(), 2, "two distinct UTC days");
        assert_eq!(series[0].day, "2026-06-10");
        assert!((series[0].cost_usd - 0.80).abs() < 1e-9);
        assert_eq!(series[0].calls, 2);
        assert_eq!(series[0].tokens, 30); // (10+5) * 2
        assert_eq!(series[1].day, "2026-06-11");
        assert!((series[1].cost_usd - 1.20).abs() < 1e-9);
    }

    #[test]
    fn daily_cost_by_dimension_splits_customers_per_day() {
        let c = conn();
        for e in [
            ev("acme", 0.50, "2026-06-10T01:00:00Z"),
            ev("heavy", 5.00, "2026-06-10T02:00:00Z"),
            ev("acme", 0.70, "2026-06-11T01:00:00Z"),
        ] {
            super::super::events::insert(&c, &e).unwrap();
        }
        let since = parse_ts("2026-06-01T00:00:00Z").unwrap();
        let until = parse_ts("2026-07-01T00:00:00Z").unwrap();
        let rows = daily_cost_by_dimension(&c, Some("p1"), "customer", since, until).unwrap();
        // (2026-06-10, acme), (2026-06-10, heavy), (2026-06-11, acme)
        assert_eq!(rows.len(), 3);
        let acme_10 = rows
            .iter()
            .find(|r| r.day == "2026-06-10" && r.key.as_deref() == Some("acme"))
            .unwrap();
        assert!((acme_10.cost_usd - 0.50).abs() < 1e-9);
        let heavy_10 = rows
            .iter()
            .find(|r| r.day == "2026-06-10" && r.key.as_deref() == Some("heavy"))
            .unwrap();
        assert!((heavy_10.cost_usd - 5.00).abs() < 1e-9);
    }
}
