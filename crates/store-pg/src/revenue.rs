//! Revenue records + LLM-cost-by-billing-dimension (profit tracking), Postgres backend.
//!
//! `metadata` is stored as a JSON string in a TEXT column (mirroring SQLite), so cost is grouped via
//! `(NULLIF(metadata,'')::jsonb)->>'customer_id'`. Summing `events.cost_usd` is COGS-correct by
//! construction: judge/benchmark spend lives in `scores`, not `events`.

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

/// Revenue rows that may be recognized within `[since, until)`. One SQL string, used by both the
/// pooled read and the transaction-scoped one below, so the admission path's revenue basis and the
/// `/v1/margin` rollup can never be reading different sets of rows.
const LIST_SQL: &str =
    "SELECT id, project_id, source, external_id, customer_id, product_id, amount_usd, currency, \
     kind, period_start, period_end, ts FROM revenue_events \
     WHERE ($1::text IS NULL OR project_id = $1) AND ( \
         (period_start IS NOT NULL AND period_end IS NOT NULL \
          AND period_start < $3 AND period_end > $2) \
      OR ((period_start IS NULL OR period_end IS NULL) AND ts >= $2 AND ts < $3) \
     ) ORDER BY ts DESC";

pub(crate) async fn list(
    pool: &PgPool,
    project: Option<&str>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<RevenueEvent>> {
    let rows = sqlx::query(LIST_SQL)
        .bind(project.map(|s| s.to_string()))
        .bind(fmt_ts(since))
        .bind(fmt_ts(until))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// [`list`] on a caller-held connection — what the admission path uses so the revenue a
/// revenue-share cap is derived from is read inside the same advisory-locked transaction as the
/// usage and the insert.
pub(crate) async fn list_in_tx(
    conn: &mut sqlx::PgConnection,
    project: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<RevenueEvent>> {
    let rows = sqlx::query(LIST_SQL)
        .bind(Some(project.to_string()))
        .bind(fmt_ts(since))
        .bind(fmt_ts(until))
        .fetch_all(&mut *conn)
        .await
        .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// The `metadata` key a margin dimension groups on. Mirrors the SQLite reference's dim map exactly
/// — an unknown dim must never silently fall through to customer data (a "prompt" margin query
/// answering with customers is wrong data).
fn dim_key(dim: &str) -> &'static str {
    match dim {
        "product" => "product_id",
        "prompt" => "prompt",
        _ => "customer_id",
    }
}

/// The guarded extraction every `metadata` read in this crate uses. `::jsonb` **raises** on invalid
/// JSON, and a raise here fails the whole margin/cost query rather than skewing one bucket — so the
/// one malformed value a hand-edited or legacy row realistically carries, the empty string, is
/// mapped to NULL first. Identical to `events::cols::USAGE_COLS` and `events::usage::scope_expr`;
/// see the note in `events/cols.rs` for why the guard stops at `NULLIF` and no wider.
const META: &str = "NULLIF(metadata,'')::jsonb";

fn cost_sql(key: &str) -> String {
    format!(
        "SELECT ({META})->>'{key}' AS k, COUNT(*)::bigint AS calls, \
         COALESCE(SUM(cost_usd),0.0) AS cost, \
         COUNT(*) FILTER (WHERE cost_usd IS NULL)::bigint AS unpriced FROM events \
         WHERE ($1::text IS NULL OR project_id = $1) AND ts >= $2 AND ts < $3 \
         GROUP BY ({META})->>'{key}'"
    )
}

pub(crate) async fn cost_by_dimension(
    pool: &PgPool,
    project: Option<&str>,
    dim: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<CostByDimension>> {
    let sql = cost_sql(dim_key(dim));
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
                unpriced_calls: row.try_get(3).map_err(pgerr)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The same three-way map the SQLite backend uses (`sqlite/revenue.rs`, modulo the `$.` JSON-path
    /// prefix). Backends disagreeing here means the same margin query answers with different data
    /// depending on which store a deployment runs.
    #[test]
    fn dim_key_matches_the_sqlite_reference() {
        assert_eq!(dim_key("product"), "product_id");
        assert_eq!(dim_key("prompt"), "prompt");
        assert_eq!(dim_key("customer"), "customer_id");
    }

    /// `by` reaches here straight from a query parameter, so an unrecognized value must land on the
    /// documented default rather than being interpolated into the SQL.
    #[test]
    fn unknown_dimension_falls_back_to_customer() {
        assert_eq!(dim_key(""), "customer_id");
        assert_eq!(dim_key("nonsense"), "customer_id");
        assert_eq!(dim_key("'; DROP TABLE events; --"), "customer_id");
    }

    /// `metadata::jsonb` **raises** on invalid JSON, so a bare cast lets one malformed row fail the
    /// whole margin query — and it read differently here than on the events path, which has always
    /// guarded with `NULLIF`. Both the projection and the `GROUP BY` must carry the guard: a
    /// `GROUP BY` expression is evaluated over every candidate row, so guarding only the projection
    /// fixes nothing. `tests/metadata_guard.rs` proves the behavior against a live Postgres.
    #[test]
    fn the_metadata_extraction_is_guarded_in_both_the_projection_and_the_group_by() {
        let sql = cost_sql("customer_id");
        assert_eq!(
            sql.matches("NULLIF(metadata,'')::jsonb").count(),
            2,
            "{sql}"
        );
        assert!(!sql.contains("(metadata::jsonb)"), "bare cast: {sql}");
    }
}
