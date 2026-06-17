//! Revenue records + LLM-cost-by-billing-dimension (profit tracking), Postgres backend.
//!
//! `metadata` is stored as a JSON string in a TEXT column (mirroring SQLite), so cost is grouped via
//! `(metadata::jsonb)->>'customer_id'`. Summing `events.cost_usd` is COGS-correct by construction:
//! judge/benchmark spend lives in `scores`, not `events`.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{CostByDimension, RevenueEvent, RevenueKind};
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

pub(crate) async fn insert(pool: &PgPool, ev: &RevenueEvent) -> Result<()> {
    // Upsert on the (deterministic, for synced records) id so webhook redelivery is idempotent.
    sqlx::query(
        "INSERT INTO revenue_events \
         (id, project_id, source, external_id, customer_id, product_id, amount_usd, currency, \
          kind, period_start, period_end, ts) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
         ON CONFLICT (id) DO UPDATE SET \
           project_id=excluded.project_id, source=excluded.source, external_id=excluded.external_id, \
           customer_id=excluded.customer_id, product_id=excluded.product_id, \
           amount_usd=excluded.amount_usd, currency=excluded.currency, kind=excluded.kind, \
           period_start=excluded.period_start, period_end=excluded.period_end, ts=excluded.ts",
    )
    .bind(ev.id.clone())
    .bind(ev.project_id.clone())
    .bind(ev.source.clone())
    .bind(ev.external_id.clone())
    .bind(ev.customer_id.clone())
    .bind(ev.product_id.clone())
    .bind(ev.amount_usd)
    .bind(ev.currency.clone())
    .bind(ev.kind.as_str())
    .bind(ev.period_start.map(fmt_ts))
    .bind(ev.period_end.map(fmt_ts))
    .bind(fmt_ts(ev.ts))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list(
    pool: &PgPool,
    project: Option<&str>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<RevenueEvent>> {
    let rows = sqlx::query(
        "SELECT id, project_id, source, external_id, customer_id, product_id, amount_usd, currency, \
         kind, period_start, period_end, ts FROM revenue_events \
         WHERE ($1::text IS NULL OR project_id = $1) AND ( \
             (period_start IS NOT NULL AND period_end IS NOT NULL \
              AND period_start < $3 AND period_end > $2) \
          OR ((period_start IS NULL OR period_end IS NULL) AND ts >= $2 AND ts < $3) \
         ) ORDER BY ts DESC",
    )
    .bind(project.map(|s| s.to_string()))
    .bind(fmt_ts(since))
    .bind(fmt_ts(until))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn cost_by_dimension(
    pool: &PgPool,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CostByDimension>> {
    let key = if dim == "product" { "product_id" } else { "customer_id" };
    let sql = format!(
        "SELECT (metadata::jsonb)->>'{key}' AS k, COUNT(*)::bigint AS calls, \
         COALESCE(SUM(cost_usd),0.0) AS cost FROM events \
         WHERE ($1::text IS NULL OR project_id = $1) AND ts >= $2 AND ts < $3 \
         GROUP BY (metadata::jsonb)->>'{key}'"
    );
    let rows = sqlx::query(&sql)
        .bind(project.map(|s| s.to_string()))
        .bind(fmt_ts(since))
        .bind(fmt_ts(until))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter()
        .map(|row| {
            Ok(CostByDimension {
                key: row.try_get(0).map_err(pgerr)?,
                calls: row.try_get(1).map_err(pgerr)?,
                cost_usd: row.try_get(2).map_err(pgerr)?,
            })
        })
        .collect()
}

fn from_row(row: &PgRow) -> Result<RevenueEvent> {
    let kind: String = row.try_get(8).map_err(pgerr)?;
    let period_start: Option<String> = row.try_get(9).map_err(pgerr)?;
    let period_end: Option<String> = row.try_get(10).map_err(pgerr)?;
    let ts: String = row.try_get(11).map_err(pgerr)?;
    Ok(RevenueEvent {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        source: row.try_get(2).map_err(pgerr)?,
        external_id: row.try_get(3).map_err(pgerr)?,
        customer_id: row.try_get(4).map_err(pgerr)?,
        product_id: row.try_get(5).map_err(pgerr)?,
        amount_usd: row.try_get(6).map_err(pgerr)?,
        currency: row.try_get(7).map_err(pgerr)?,
        kind: RevenueKind::parse(&kind),
        period_start: match period_start {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        period_end: match period_end {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        ts: parse_ts(&ts)?,
    })
}
