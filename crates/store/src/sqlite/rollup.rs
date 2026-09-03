//! The one grouped rollup, SQLite: a `GROUP BY` built from [`Dimension`] rather than five
//! near-identical hand-written statements.
//!
//! Every interpolated fragment is a **fixed literal chosen by an enum variant** — never caller text.
//! Values (the project, the window bounds, the filter values) are always bound as parameters, so a
//! `?by=` or `?filter=` from the wire cannot reach the SQL. The project predicate reuses
//! [`super::project_pred`] so a concrete project still seeks `idx_events_project_ts`; the plan-shape
//! test in `sqlite/revenue.rs` pins that property and this builder must not lose it.

use rusqlite::types::ToSql;
use rusqlite::{params_from_iter, Connection, Row};

use lighttrack_core::{Dimension, RollupQuery, RollupRow, Storage, TimeKey};

use crate::codec::fmt_ts;
use crate::{Result, StoreError};

/// The SQL expression yielding one dimension's value for a row, under `time` as the window key.
fn key_expr(d: Dimension, time: &str) -> String {
    match d.storage() {
        Storage::Column(c) => c.to_string(),
        Storage::MetadataKey(k) => format!("json_extract(metadata,'$.{k}')"),
        Storage::Day => format!("substr({time},1,10)"),
    }
}

/// The window/day key. `received_at` is the accounting key (server arrival); `COALESCE` keeps rows
/// written before the column existed counted at their `ts`.
fn time_expr(k: TimeKey) -> &'static str {
    match k {
        TimeKey::Ts => "ts",
        TimeKey::ReceivedAt => "COALESCE(received_at, ts)",
    }
}

pub(super) fn rollup(conn: &Connection, q: &RollupQuery<'_>) -> Result<Vec<RollupRow>> {
    if let Some(why) = q.invalid() {
        return Err(StoreError::Other(why));
    }
    let time = time_expr(q.time_key);
    let keys: Vec<String> = q.group_by.iter().map(|d| key_expr(*d, time)).collect();

    // `?1` is the project (bound in both arms of `project_pred`), `?2` the window start; every
    // further placeholder is appended in the order its value is pushed.
    let mut args: Vec<Box<dyn ToSql>> = vec![
        Box::new(q.project.map(str::to_string)),
        Box::new(fmt_ts(q.since)),
    ];
    let mut conds = vec![
        super::project_pred(q.project).to_string(),
        format!("{time} >= ?2"),
    ];
    if let Some(u) = q.until {
        args.push(Box::new(fmt_ts(u)));
        conds.push(format!("{time} < ?{}", args.len()));
    }
    if q.unpriced_only {
        conds.push("cost_usd IS NULL".to_string());
    }
    for (d, v) in &q.filter {
        args.push(Box::new(v.clone()));
        conds.push(format!("{} = ?{}", key_expr(*d, time), args.len()));
    }

    let select: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{e} AS k{i}"))
        .collect();
    let group: Vec<String> = (1..=keys.len()).map(|i| i.to_string()).collect();
    let sql = format!(
        "SELECT {sel}, COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
         COALESCE(SUM(cost_usd),0.0), \
         COALESCE(SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END),0), \
         COALESCE(SUM(CASE WHEN json_extract(metadata,'$.cost_source') = 'client' \
                           THEN cost_usd ELSE 0 END),0.0), \
         COALESCE(SUM(CASE WHEN status <> 'success' THEN 1 ELSE 0 END),0) \
         FROM events WHERE {conds} GROUP BY {group}",
        sel = select.join(", "),
        conds = conds.join(" AND "),
        group = group.join(", "),
    );

    let n = keys.len();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), |r: &Row| map_row(r, n))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn map_row(r: &Row, n: usize) -> rusqlite::Result<RollupRow> {
    let mut keys = Vec::with_capacity(n);
    for i in 0..n {
        keys.push(r.get::<_, Option<String>>(i)?);
    }
    // SQLite counts and sums are `i64`; they are non-negative by construction here (COUNT, and SUMs
    // over non-negative token columns), so the clamp is belt-and-braces rather than a conversion.
    let u = |v: i64| v.max(0) as u64;
    Ok(RollupRow {
        keys,
        calls: u(r.get(n)?),
        input_tokens: u(r.get(n + 1)?),
        output_tokens: u(r.get(n + 2)?),
        cost_usd: r.get(n + 3)?,
        unpriced_calls: u(r.get(n + 4)?),
        client_reported_cost_usd: r.get(n + 5)?,
        errors: u(r.get(n + 6)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::parse_ts;
    use chrono::{DateTime, Utc};
    use lighttrack_core::LlmEvent;
    use serde_json::json;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().expect("in-memory db");
        crate::sqlite::schema::apply(&c).expect("schema");
        c
    }

    fn ev(id: &str, ts: &str, cost: Option<f64>, meta: serde_json::Value) -> LlmEvent {
        let mut e: LlmEvent = serde_json::from_value(json!({
            "id": id, "project_id": "p1", "provider": "anthropic",
            "model": "claude-haiku-4-5", "name": "chat", "ts": ts,
            "usage": { "input": 10, "output": 5 }, "metadata": meta,
        }))
        .expect("fixture");
        e.cost_usd = cost;
        e.received_at = e.ts;
        e
    }

    fn seed(c: &Connection) {
        for e in [
            ev(
                "a",
                "2026-06-10T01:00:00Z",
                Some(1.0),
                json!({"customer_id":"acme"}),
            ),
            ev(
                "b",
                "2026-06-10T02:00:00Z",
                Some(2.0),
                json!({"customer_id":"acme"}),
            ),
            // Unpriced: no `cost_usd` on the row at all.
            ev(
                "c",
                "2026-06-11T01:00:00Z",
                None,
                json!({"customer_id":"acme"}),
            ),
            ev(
                "d",
                "2026-06-11T02:00:00Z",
                Some(4.0),
                json!({"customer_id":"heavy"}),
            ),
            // Untagged — folds into the NULL bucket, never dropped.
            ev("e", "2026-06-11T03:00:00Z", Some(8.0), json!({})),
        ] {
            super::super::events::insert(c, &e).expect("insert");
        }
    }

    fn win() -> (DateTime<Utc>, DateTime<Utc>) {
        (
            parse_ts("2026-06-01T00:00:00Z").expect("since"),
            parse_ts("2026-07-01T00:00:00Z").expect("until"),
        )
    }

    #[test]
    fn groups_by_a_metadata_dimension_and_discloses_unpriced_calls() {
        let c = conn();
        seed(&c);
        let (s, u) = win();
        let q = RollupQuery::new(&[Dimension::Customer], s)
            .project(Some("p1"))
            .until(Some(u));
        let rows = rollup(&c, &q).expect("rollup");

        let acme = rows
            .iter()
            .find(|r| r.key(0) == Some("acme"))
            .expect("acme bucket");
        assert_eq!(acme.calls, 3);
        assert!((acme.cost_usd - 3.0).abs() < 1e-9, "stored sum only");
        assert_eq!(
            acme.unpriced_calls, 1,
            "the third call had no price — the $3.00 is a floor, and the row says so"
        );
        assert_eq!(acme.tokens(), 45);

        let untagged = rows.iter().find(|r| r.key(0).is_none()).expect("null key");
        assert!((untagged.cost_usd - 8.0).abs() < 1e-9);
        assert_eq!(
            rows.iter().map(|r| r.calls).sum::<u64>(),
            5,
            "the parts sum to the window — no bucket is dropped"
        );
    }

    #[test]
    fn day_buckets_split_on_the_chosen_time_key() {
        let c = conn();
        seed(&c);
        let (s, u) = win();
        for key in [TimeKey::Ts, TimeKey::ReceivedAt] {
            let q = RollupQuery::new(&[Dimension::Day], s)
                .project(Some("p1"))
                .until(Some(u))
                .time_key(key);
            let rows = rollup(&c, &q).expect("rollup");
            assert_eq!(rows.len(), 2, "two UTC days, {key:?}");
            let d10 = rows
                .iter()
                .find(|r| r.key(0) == Some("2026-06-10"))
                .expect("first day");
            assert_eq!(d10.calls, 2);
            assert!((d10.cost_usd - 3.0).abs() < 1e-9);
        }
    }

    #[test]
    fn a_filter_scopes_the_rollup_and_never_leaks_another_value() {
        let c = conn();
        seed(&c);
        let (s, u) = win();
        let q = RollupQuery::new(&[Dimension::Provider, Dimension::Model], s)
            .project(Some("p1"))
            .until(Some(u))
            .filter(Dimension::Customer, "heavy");
        let rows = rollup(&c, &q).expect("rollup");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls, 1);
        assert!((rows[0].cost_usd - 4.0).abs() < 1e-9, "heavy's spend only");

        // A customer with no traffic is empty, never the project total.
        let none = rollup(
            &c,
            &RollupQuery::new(&[Dimension::Model], s)
                .project(Some("p1"))
                .until(Some(u))
                .filter(Dimension::Customer, "nobody"),
        )
        .expect("rollup");
        assert!(none.is_empty());
    }

    /// `unpriced_only` must narrow the ROWS, not just the disclosure column: the ledger's token
    /// sums are the unpriced traffic's, and a bucket with no unpriced call must vanish entirely
    /// rather than come back with `calls: 0`.
    #[test]
    fn unpriced_only_narrows_the_rows_it_sums() {
        let c = conn();
        seed(&c);
        let (s, u) = win();
        let q = |only| {
            let q = RollupQuery::new(&[Dimension::Model], s)
                .project(Some("p1"))
                .until(Some(u));
            rollup(&c, &if only { q.only_unpriced() } else { q }).expect("rollup")
        };

        let all = q(false);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].calls, 5);
        assert_eq!(all[0].unpriced_calls, 1);
        assert_eq!(all[0].tokens(), 75);

        let unpriced = q(true);
        assert_eq!(unpriced.len(), 1);
        assert_eq!(unpriced[0].calls, 1, "only the NULL-cost call");
        assert_eq!(unpriced[0].tokens(), 15, "and only ITS tokens");
        assert!((unpriced[0].cost_usd - 0.0).abs() < 1e-12);

        // A window with nothing unpriced comes back empty, not as the whole bucket at zero.
        let none = rollup(
            &c,
            &RollupQuery::new(&[Dimension::Model], s)
                .project(Some("p1"))
                .until(Some(u))
                .filter(Dimension::Customer, "heavy")
                .only_unpriced(),
        )
        .expect("rollup");
        assert!(none.is_empty());
    }

    /// A malformed query is refused before any SQL is built — the shared validation, so every
    /// backend answers the same request the same way.
    #[test]
    fn a_malformed_query_is_refused_not_answered() {
        let c = conn();
        let (s, _) = win();
        assert!(rollup(&c, &RollupQuery::new(&[], s)).is_err());
        assert!(rollup(
            &c,
            &RollupQuery::new(&[Dimension::Model, Dimension::Model], s)
        )
        .is_err());
    }

    /// The sargable form must survive the builder: a concrete project has to seek
    /// `idx_events_project_ts`, not full-scan every project's whole history under the write mutex.
    #[test]
    fn a_concrete_project_still_rides_the_project_index() {
        let c = conn();
        let (s, u) = win();
        let q = RollupQuery::new(&[Dimension::Customer], s)
            .project(Some("p1"))
            .until(Some(u));
        // Rebuild the statement the same way `rollup` does, then explain it.
        let time = time_expr(q.time_key);
        let sql = format!(
            "SELECT {k} AS k0, COUNT(*) FROM events WHERE {p} AND {time} >= ?2 AND {time} < ?3 \
             GROUP BY 1",
            k = key_expr(Dimension::Customer, time),
            p = super::super::project_pred(q.project),
        );
        let mut stmt = c
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("explain");
        let plan = stmt
            .query_map(
                rusqlite::params!["p1", "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"],
                |r| r.get::<_, String>(3),
            )
            .expect("plan")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("plan rows")
            .join(" | ");
        assert!(
            plan.contains("idx_events_project_ts"),
            "the builder lost the sargable project predicate; plan: {plan}"
        );
    }
}
