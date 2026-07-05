//! Revenue records + LLM-cost-by-billing-dimension (Phase 1 profit tracking).
//!
//! Cost is grouped by `json_extract(metadata, '$.customer_id'|'$.product_id')` — the billing linkage
//! rides in the event `metadata` blob, so no events-schema change is needed. Summing `events.cost_usd`
//! is COGS-correct by construction: judge/benchmark spend lives in `scores`, not `events`.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use lighttrack_core::{CostByDimension, RevenueEvent, RevenueKind};

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

pub(super) fn insert(conn: &Connection, ev: &RevenueEvent) -> Result<()> {
    // Upsert on the (deterministic, for synced records) id so webhook redelivery is idempotent —
    // Stripe retries any non-2xx, so a re-sent event must not duplicate or error.
    conn.execute(
        "INSERT INTO revenue_events \
         (id, project_id, source, external_id, customer_id, product_id, amount_usd, currency, \
          kind, period_start, period_end, ts) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
         ON CONFLICT(id) DO UPDATE SET \
           project_id=excluded.project_id, source=excluded.source, external_id=excluded.external_id, \
           customer_id=excluded.customer_id, product_id=excluded.product_id, \
           amount_usd=excluded.amount_usd, currency=excluded.currency, kind=excluded.kind, \
           period_start=excluded.period_start, period_end=excluded.period_end, ts=excluded.ts",
        params![
            ev.id,
            ev.project_id,
            ev.source,
            ev.external_id,
            ev.customer_id,
            ev.product_id,
            ev.amount_usd,
            ev.currency,
            ev.kind.as_str(),
            ev.period_start.map(fmt_ts),
            ev.period_end.map(fmt_ts),
            fmt_ts(ev.ts),
        ],
    )?;
    Ok(())
}

/// Persist a batch of revenue records atomically: one transaction wraps the per-record upserts, so a
/// mid-batch failure rolls the whole batch back instead of committing a partial prefix. Safe to retry
/// on a webhook redelivery — each upsert is idempotent on `id`. `unchecked_transaction` is sound here
/// because `SqliteStore` already holds the connection mutex, so this is the only writer.
pub(super) fn insert_batch(conn: &Connection, evs: &[RevenueEvent]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for ev in evs {
        insert(&tx, ev)?;
    }
    tx.commit()?;
    Ok(())
}

/// Revenue records that could be recognized within `[since, until)`: period events overlapping the
/// window, plus point-in-time events with `ts` in range. Exact recognition (amortization) is the
/// caller's job (`core::compute_margin`).
pub(super) fn list(
    conn: &Connection,
    project: Option<&str>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<RevenueEvent>> {
    let sql = "SELECT id, project_id, source, external_id, customer_id, product_id, amount_usd, \
               currency, kind, period_start, period_end, ts \
               FROM revenue_events \
               WHERE (?1 IS NULL OR project_id = ?1) AND ( \
                   (period_start IS NOT NULL AND period_end IS NOT NULL \
                    AND period_start < ?3 AND period_end > ?2) \
                OR ((period_start IS NULL OR period_end IS NULL) AND ts >= ?2 AND ts < ?3) \
               ) ORDER BY ts DESC";
    let mut stmt = conn.prepare(sql)?;
    let raws = stmt
        .query_map(params![project, fmt_ts(since), fmt_ts(until)], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// LLM cost grouped by a billing dimension (`customer` | `product`), read from event metadata, over
/// `[since, until)`. Untagged calls group under a NULL key (`unattributed`).
pub(super) fn cost_by_dimension(
    conn: &Connection,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CostByDimension>> {
    let path = match dim {
        "product" => "$.product_id",
        _ => "$.customer_id",
    };
    let sql = format!(
        "SELECT json_extract(metadata, '{path}') AS k, COUNT(*) AS calls, \
         COALESCE(SUM(cost_usd),0.0) AS cost \
         FROM events \
         WHERE (?1 IS NULL OR project_id = ?1) AND ts >= ?2 AND ts < ?3 \
         GROUP BY k"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![project, fmt_ts(since), fmt_ts(until)], |row: &Row| {
            Ok(CostByDimension {
                key: row.get::<_, Option<String>>(0)?,
                calls: row.get(1)?,
                cost_usd: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

struct RawRevenue {
    id: String,
    project_id: String,
    source: String,
    external_id: Option<String>,
    customer_id: Option<String>,
    product_id: Option<String>,
    amount_usd: f64,
    currency: String,
    kind: String,
    period_start: Option<String>,
    period_end: Option<String>,
    ts: String,
}

fn map_raw(row: &Row) -> rusqlite::Result<RawRevenue> {
    Ok(RawRevenue {
        id: row.get(0)?,
        project_id: row.get(1)?,
        source: row.get(2)?,
        external_id: row.get(3)?,
        customer_id: row.get(4)?,
        product_id: row.get(5)?,
        amount_usd: row.get(6)?,
        currency: row.get(7)?,
        kind: row.get(8)?,
        period_start: row.get(9)?,
        period_end: row.get(10)?,
        ts: row.get(11)?,
    })
}

fn from_raw(r: RawRevenue) -> Result<RevenueEvent> {
    Ok(RevenueEvent {
        id: r.id,
        project_id: r.project_id,
        source: r.source,
        external_id: r.external_id,
        customer_id: r.customer_id,
        product_id: r.product_id,
        amount_usd: r.amount_usd,
        currency: r.currency,
        kind: RevenueKind::parse(&r.kind),
        period_start: r.period_start.as_deref().map(parse_ts).transpose()?,
        period_end: r.period_end.as_deref().map(parse_ts).transpose()?,
        ts: parse_ts(&r.ts)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{compute_margin, LlmEvent, MarginDimension};
    use serde_json::json;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../../../schema/sqlite/001_init.sql")).unwrap();
        c
    }

    fn ev(customer: &str, cost: f64, ts: &str) -> LlmEvent {
        serde_json::from_value(json!({
            "id": format!("e-{customer}-{ts}"), "project_id": "p1",
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "ts": ts, "cost_usd": cost, "metadata": { "customer_id": customer }
        }))
        .unwrap()
    }

    #[test]
    fn end_to_end_margin_over_store() {
        let c = conn();
        // Two customers' monitored traffic.
        for e in [
            ev("acme", 0.50, "2026-06-10T00:00:00Z"),
            ev("acme", 0.37, "2026-06-11T00:00:00Z"),
            ev("heavy", 142.5, "2026-06-12T00:00:00Z"),
        ] {
            super::super::events::insert(&c, &e).unwrap();
        }
        // Revenue: acme pays $20, heavy pays $99.
        for r in [
            RevenueEvent {
                id: "r1".into(), project_id: "p1".into(), source: "manual".into(),
                external_id: None, customer_id: Some("acme".into()), product_id: None,
                amount_usd: 20.0, currency: "USD".into(), kind: RevenueKind::OneTime,
                period_start: None, period_end: None, ts: parse_ts("2026-06-10T00:00:00Z").unwrap(),
            },
            RevenueEvent {
                id: "r2".into(), project_id: "p1".into(), source: "manual".into(),
                external_id: None, customer_id: Some("heavy".into()), product_id: None,
                amount_usd: 99.0, currency: "USD".into(), kind: RevenueKind::OneTime,
                period_start: None, period_end: None, ts: parse_ts("2026-06-12T00:00:00Z").unwrap(),
            },
        ] {
            insert(&c, &r).unwrap();
        }

        let since = parse_ts("2026-06-01T00:00:00Z").unwrap();
        let until = parse_ts("2026-07-01T00:00:00Z").unwrap();
        let revenue = list(&c, Some("p1"), since, until).unwrap();
        let costs = cost_by_dimension(&c, Some("p1"), "customer", since, until).unwrap();
        let rows = compute_margin(&revenue, &costs, MarginDimension::Customer, since, until);

        // heavy is the money-loser → first; acme is healthy.
        assert_eq!(rows[0].key, "heavy");
        assert!((rows[0].gross_margin_usd - (99.0 - 142.5)).abs() < 1e-6);
        let acme = rows.iter().find(|r| r.key == "acme").unwrap();
        assert!((acme.llm_cost_usd - 0.87).abs() < 1e-9);
        assert!((acme.gross_margin_usd - 19.13).abs() < 1e-9);
        assert_eq!(acme.calls, 2);
    }

    fn rev(id: &str, amount: f64) -> RevenueEvent {
        RevenueEvent {
            id: id.into(), project_id: "p1".into(), source: "stripe".into(),
            external_id: Some(format!("ext-{id}")), customer_id: Some("acme".into()),
            product_id: None, amount_usd: amount, currency: "USD".into(), kind: RevenueKind::OneTime,
            period_start: None, period_end: None, ts: parse_ts("2026-06-10T00:00:00Z").unwrap(),
        }
    }

    fn all(c: &Connection) -> Vec<RevenueEvent> {
        let since = parse_ts("2026-01-01T00:00:00Z").unwrap();
        let until = parse_ts("2027-01-01T00:00:00Z").unwrap();
        list(c, Some("p1"), since, until).unwrap()
    }

    #[test]
    fn batch_is_atomic_all_or_nothing() {
        let c = conn();
        // Force a deterministic mid-batch failure: any insert of id='bad' aborts.
        c.execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON revenue_events \
             WHEN NEW.id = 'bad' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
        .unwrap();

        // A batch whose second record fails rolls the whole batch back — the good first record is
        // NOT left committed (the old per-event loop would have stranded it).
        let err = insert_batch(&c, &[rev("good", 10.0), rev("bad", 5.0)]);
        assert!(err.is_err(), "a failing record must error the batch");
        assert!(all(&c).is_empty(), "rolled-back batch left a partial write");

        // A fully-valid batch commits every record.
        insert_batch(&c, &[rev("good", 10.0), rev("good2", 7.0)]).unwrap();
        assert_eq!(all(&c).len(), 2);
    }

    #[test]
    fn batch_redelivery_is_idempotent() {
        let c = conn();
        let batch = [rev("r1", 10.0), rev("r2", 7.0)];
        insert_batch(&c, &batch).unwrap();
        // Same delivery again (provider retry): upsert-on-id means no duplicates accrue.
        insert_batch(&c, &batch).unwrap();
        assert_eq!(all(&c).len(), 2);
    }
}
