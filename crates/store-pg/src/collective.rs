//! Collective Model Intelligence — the shared leaderboard's entry table, Postgres backend.
//!
//! Mirrors `lighttrack-store`'s SQLite reference (`sqlite/collective.rs`) exactly: pure aggregate
//! rows keyed on `(contributor_id, provider, model, task_type)`, timestamps as fixed-width
//! `RFC3339(Nanos, Z)` TEXT so the retention range filter is a correct chronological one.
//!
//! The one thing this backend does that SQLite also does — and that the API's old delete-then-N-
//! upserts loop did not — is [`replace`]: the whole contributor set turns over inside a single
//! transaction, so an interrupted hub ingest can never publish a half-replaced contributor.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Executor, Postgres, Row};

use lighttrack_core::{CollectiveEntry, Coverage};
use lighttrack_store::{CollectiveFilter, ReplaceAck, Result};

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd, \
     p50_latency_ms, p95_latency_ms, n_runs, n_cases, quality_variance, judge_provider, \
     rubric_fingerprint, determinism, frozen_dataset, significance_tested, received_at";

const UPSERT: &str = "INSERT INTO collective_entries \
     (contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd, \
      p50_latency_ms, p95_latency_ms, n_runs, n_cases, quality_variance, judge_provider, \
      rubric_fingerprint, determinism, frozen_dataset, significance_tested, received_at) \
     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18) \
     ON CONFLICT (contributor_id, provider, model, task_type) DO UPDATE SET \
       quality=excluded.quality, pass_rate=excluded.pass_rate, avg_cost_usd=excluded.avg_cost_usd, \
       p50_latency_ms=excluded.p50_latency_ms, p95_latency_ms=excluded.p95_latency_ms, \
       n_runs=excluded.n_runs, n_cases=excluded.n_cases, \
       quality_variance=excluded.quality_variance, judge_provider=excluded.judge_provider, \
       rubric_fingerprint=excluded.rubric_fingerprint, determinism=excluded.determinism, \
       frozen_dataset=excluded.frozen_dataset, \
       significance_tested=excluded.significance_tested, received_at=excluded.received_at";

/// Bind one entry onto a prepared `UPSERT`, so the standalone upsert and the one inside a
/// transaction cannot drift apart in their column order.
fn bind_upsert<'q>(
    e: &'q CollectiveEntry,
) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(UPSERT)
        .bind(&e.contributor_id)
        .bind(&e.provider)
        .bind(&e.model)
        .bind(&e.task_type)
        .bind(e.quality)
        .bind(e.pass_rate)
        .bind(e.avg_cost_usd)
        .bind(e.p50_latency_ms.map(|v| v as i64))
        .bind(e.p95_latency_ms.map(|v| v as i64))
        .bind(e.n_runs as i64)
        .bind(e.n_cases as i64)
        .bind(e.quality_variance)
        .bind(&e.judge_provider)
        .bind(&e.rubric_fingerprint)
        .bind(&e.determinism)
        .bind(e.frozen_dataset.to_tag())
        .bind(e.significance_tested.to_tag())
        .bind(fmt_ts(e.received_at))
}

pub(crate) async fn upsert(pool: &PgPool, e: &CollectiveEntry) -> Result<()> {
    bind_upsert(e).execute(pool).await.map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn delete(pool: &PgPool, contributor_id: &str) -> Result<u64> {
    let done = sqlx::query("DELETE FROM collective_entries WHERE contributor_id = $1")
        .bind(contributor_id)
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(done.rows_affected())
}

pub(crate) async fn purge_before(pool: &PgPool, cutoff: DateTime<Utc>) -> Result<u64> {
    let done = sqlx::query("DELETE FROM collective_entries WHERE received_at < $1")
        .bind(fmt_ts(cutoff))
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(done.rows_affected())
}

pub(crate) async fn list(pool: &PgPool) -> Result<Vec<CollectiveEntry>> {
    let sql = format!("SELECT {COLS} FROM collective_entries");
    let rows = sqlx::query(&sql).fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// Retention-narrowed read (`idx_collective_received`). Only pre-floor-safe predicates reach the
/// store — see `CollectiveFilter` for why a provider/task filter must not.
pub(crate) async fn list_filtered(
    pool: &PgPool,
    f: &CollectiveFilter,
) -> Result<Vec<CollectiveEntry>> {
    let Some(after) = f.received_after else {
        return list(pool).await;
    };
    let sql = format!("SELECT {COLS} FROM collective_entries WHERE received_at >= $1");
    let rows = sqlx::query(&sql)
        .bind(fmt_ts(after))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// `MAX(received_at)` for one contributor — a keyed read on the primary key's leading column, so the
/// ingest rate limit costs an index probe rather than a decode of the whole table.
pub(crate) async fn latest_receipt(
    pool: &PgPool,
    contributor_id: &str,
) -> Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT MAX(received_at) FROM collective_entries WHERE contributor_id = $1",
    )
    .bind(contributor_id)
    .fetch_one(pool)
    .await
    .map_err(pgerr)?;
    raw.map(|s| parse_ts(&s)).transpose()
}

/// Replace a contributor's whole set (and optionally sweep retention) in ONE transaction: on any
/// error the previous set is still there, rather than a mixture of old and new rows that the merged
/// leaderboard would publish as the collective's opinion.
pub(crate) async fn replace(
    pool: &PgPool,
    contributor_id: &str,
    entries: &[CollectiveEntry],
    purge_before: Option<DateTime<Utc>>,
) -> Result<ReplaceAck> {
    let mut tx = pool.begin().await.map_err(pgerr)?;
    let deleted = sqlx::query("DELETE FROM collective_entries WHERE contributor_id = $1")
        .bind(contributor_id)
        .execute(&mut *tx)
        .await
        .map_err(pgerr)?
        .rows_affected();
    for e in entries {
        tx.execute(bind_upsert(e)).await.map_err(pgerr)?;
    }
    // Same cutoff semantics as the standalone sweep, and last: the hub stamps `received_at = now`
    // on every entry it accepts, so the set just written is never older than the cutoff.
    let purged = match purge_before {
        Some(c) => sqlx::query("DELETE FROM collective_entries WHERE received_at < $1")
            .bind(fmt_ts(c))
            .execute(&mut *tx)
            .await
            .map_err(pgerr)?
            .rows_affected(),
        None => 0,
    };
    tx.commit().await.map_err(pgerr)?;
    Ok(ReplaceAck {
        deleted,
        inserted: entries.len() as u64,
        purged,
        atomic: true,
    })
}

fn cov(tag: Option<String>) -> Coverage {
    tag.as_deref()
        .map(Coverage::from_tag)
        .unwrap_or(Coverage::Unknown)
}

fn from_row(row: &PgRow) -> Result<CollectiveEntry> {
    let p50: Option<i64> = row.try_get(7).map_err(pgerr)?;
    let p95: Option<i64> = row.try_get(8).map_err(pgerr)?;
    let n_runs: i64 = row.try_get(9).map_err(pgerr)?;
    let n_cases: i64 = row.try_get(10).map_err(pgerr)?;
    let frozen: Option<String> = row.try_get(15).map_err(pgerr)?;
    let significance: Option<String> = row.try_get(16).map_err(pgerr)?;
    let received_at: String = row.try_get(17).map_err(pgerr)?;
    Ok(CollectiveEntry {
        contributor_id: row.try_get(0).map_err(pgerr)?,
        provider: row.try_get(1).map_err(pgerr)?,
        model: row.try_get(2).map_err(pgerr)?,
        task_type: row.try_get(3).map_err(pgerr)?,
        quality: row.try_get(4).map_err(pgerr)?,
        pass_rate: row.try_get(5).map_err(pgerr)?,
        avg_cost_usd: row.try_get(6).map_err(pgerr)?,
        p50_latency_ms: p50.map(|v| v as u64),
        p95_latency_ms: p95.map(|v| v as u64),
        n_runs: n_runs as u32,
        n_cases: n_cases as u32,
        quality_variance: row.try_get(11).map_err(pgerr)?,
        judge_provider: row.try_get(12).map_err(pgerr)?,
        rubric_fingerprint: row.try_get(13).map_err(pgerr)?,
        determinism: row.try_get(14).map_err(pgerr)?,
        // A row written before the rigor columns existed stored NULL, which reads back as
        // `Unknown` — no backfill, same as SQLite.
        frozen_dataset: cov(frozen),
        significance_tested: cov(significance),
        received_at: parse_ts(&received_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    /// `from_row` reads by position, so the projection and the reader must agree. They are far apart
    /// in the file, which is exactly how an off-by-one column read gets introduced.
    #[test]
    fn the_projection_matches_the_positional_reader() {
        let names = select_list_names(COLS);
        assert_eq!(names.len(), 18);
        assert_eq!(names[7], "p50_latency_ms");
        assert_eq!(names[9], "n_runs");
        assert_eq!(names[15], "frozen_dataset");
        assert_eq!(names[17], "received_at");
    }

    /// The upsert binds 18 parameters and the ON CONFLICT target must be the full primary key —
    /// a narrower target would let one contributor hold several rows for the same model+task and
    /// outvote everyone else, the exact failure the key exists to prevent.
    #[test]
    fn the_upsert_keys_on_the_whole_primary_key() {
        assert!(
            UPSERT.contains("ON CONFLICT (contributor_id, provider, model, task_type)"),
            "{UPSERT}"
        );
        assert!(UPSERT.contains("$18") && !UPSERT.contains("$19"));
    }
}
