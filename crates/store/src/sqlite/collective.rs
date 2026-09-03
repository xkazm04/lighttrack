//! Collective Model Intelligence — hub-side storage of contributed, privacy-safe digest entries.
//!
//! Rows are pure aggregate (no text, no project/customer ids); the primary key
//! `(contributor_id, provider, model, task_type)` makes a re-contribution upsert in place, and
//! `delete` lets the ingest handler replace a contributor's whole set so dropped buckets don't linger.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

use lighttrack_core::{CollectiveEntry, Coverage};

use crate::codec::{fmt_ts, parse_ts};
use crate::{CollectiveFilter, ReplaceAck, Result};

pub(super) fn upsert(conn: &Connection, e: &CollectiveEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO collective_entries \
         (contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd, \
          p50_latency_ms, p95_latency_ms, n_runs, n_cases, quality_variance, \
          judge_provider, rubric_fingerprint, determinism, frozen_dataset, significance_tested, \
          received_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18) \
         ON CONFLICT(contributor_id, provider, model, task_type) DO UPDATE SET \
           quality=excluded.quality, pass_rate=excluded.pass_rate, avg_cost_usd=excluded.avg_cost_usd, \
           p50_latency_ms=excluded.p50_latency_ms, p95_latency_ms=excluded.p95_latency_ms, \
           n_runs=excluded.n_runs, n_cases=excluded.n_cases, \
           quality_variance=excluded.quality_variance, judge_provider=excluded.judge_provider, \
           rubric_fingerprint=excluded.rubric_fingerprint, determinism=excluded.determinism, \
           frozen_dataset=excluded.frozen_dataset, \
           significance_tested=excluded.significance_tested, received_at=excluded.received_at",
        params![
            e.contributor_id,
            e.provider,
            e.model,
            e.task_type,
            e.quality,
            e.pass_rate,
            e.avg_cost_usd,
            e.p50_latency_ms.map(|v| v as i64),
            e.p95_latency_ms.map(|v| v as i64),
            e.n_runs as i64,
            e.n_cases as i64,
            e.quality_variance,
            e.judge_provider,
            e.rubric_fingerprint,
            e.determinism,
            e.frozen_dataset.to_tag(),
            e.significance_tested.to_tag(),
            fmt_ts(e.received_at),
        ],
    )?;
    Ok(())
}

/// Remove every entry from `contributor_id`. Returns how many rows were deleted.
pub(super) fn delete(conn: &Connection, contributor_id: &str) -> Result<u64> {
    let n = conn.execute(
        "DELETE FROM collective_entries WHERE contributor_id = ?1",
        params![contributor_id],
    )?;
    Ok(n as u64)
}

/// Retention sweep: drop entries received before `cutoff`. Timestamps are fixed-width
/// `RFC3339(Nanos, Z)`, so the string comparison is a correct chronological one.
pub(super) fn purge_before(
    conn: &Connection,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<u64> {
    let n = conn.execute(
        "DELETE FROM collective_entries WHERE received_at < ?1",
        params![fmt_ts(cutoff)],
    )?;
    Ok(n as u64)
}

/// In the schema model's column order (`received_at` shipped with the table; the v2/v3 columns were
/// added after it), which the test below holds it to - `map_raw` reads by position.
const COLS: &str = "contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd, \
     p50_latency_ms, p95_latency_ms, n_runs, n_cases, received_at, quality_variance, \
     judge_provider, rubric_fingerprint, determinism, frozen_dataset, significance_tested";

pub(super) fn list(conn: &Connection) -> Result<Vec<CollectiveEntry>> {
    let sql = format!("SELECT {COLS} FROM collective_entries");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map([], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Retention-narrowed read. `received_at` is fixed-width `RFC3339(Nanos, Z)`, so the `>=` is a
/// correct chronological comparison and rides `idx_collective_received` instead of decoding every
/// row into a struct only to drop most of them in the handler.
pub(super) fn list_filtered(
    conn: &Connection,
    f: &CollectiveFilter,
) -> Result<Vec<CollectiveEntry>> {
    let Some(after) = f.received_after else {
        return list(conn);
    };
    let sql = format!("SELECT {COLS} FROM collective_entries WHERE received_at >= ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![fmt_ts(after)], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Newest receipt for one contributor. A keyed `MAX` on the primary key's leading column — the
/// ingest rate limit used to answer this by decoding the whole table.
pub(super) fn latest_receipt(
    conn: &Connection,
    contributor_id: &str,
) -> Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = conn.query_row(
        "SELECT MAX(received_at) FROM collective_entries WHERE contributor_id = ?1",
        params![contributor_id],
        |r| r.get(0),
    )?;
    raw.map(|s| parse_ts(&s)).transpose()
}

/// Replace a contributor's whole set (and optionally sweep retention) in **one** transaction, so a
/// failure part-way through leaves the previous set intact rather than a half-replaced one. Nothing
/// else can interleave: `SqliteStore` already holds the single write connection, which is why
/// `unchecked_transaction` is sound here (same argument as `revenue::insert_batch`).
pub(super) fn replace(
    conn: &Connection,
    contributor_id: &str,
    entries: &[CollectiveEntry],
    purge_before: Option<DateTime<Utc>>,
) -> Result<ReplaceAck> {
    let tx = conn.unchecked_transaction()?;
    let ack = apply_replace(&tx, contributor_id, entries, purge_before)?;
    tx.commit()?;
    Ok(ack)
}

/// The body of [`replace`], factored out so a test can run it inside a transaction it then drops —
/// which is the only way to prove the rollback rather than assert it in a comment.
pub(super) fn apply_replace(
    conn: &Connection,
    contributor_id: &str,
    entries: &[CollectiveEntry],
    purge_before: Option<DateTime<Utc>>,
) -> Result<ReplaceAck> {
    let deleted = delete(conn, contributor_id)?;
    for e in entries {
        upsert(conn, e)?;
    }
    // The sweep is the standalone one, unchanged, so `replace` cannot drift from
    // `purge_collective_entries_before`. It runs last: the hub stamps `received_at = now` on every
    // entry it accepts, so the set just written is never older than the cutoff.
    let purged = match purge_before {
        Some(c) => self::purge_before(conn, c)?,
        None => 0,
    };
    Ok(ReplaceAck {
        deleted,
        inserted: entries.len() as u64,
        purged,
        atomic: true,
    })
}

struct Raw {
    contributor_id: String,
    provider: String,
    model: String,
    task_type: String,
    quality: f64,
    pass_rate: f64,
    avg_cost_usd: f64,
    p50_latency_ms: Option<i64>,
    p95_latency_ms: Option<i64>,
    n_runs: i64,
    n_cases: i64,
    quality_variance: Option<f64>,
    judge_provider: Option<String>,
    rubric_fingerprint: Option<String>,
    determinism: Option<String>,
    frozen_dataset: Option<String>,
    significance_tested: Option<String>,
    received_at: String,
}

fn map_raw(row: &Row) -> rusqlite::Result<Raw> {
    Ok(Raw {
        contributor_id: row.get(0)?,
        provider: row.get(1)?,
        model: row.get(2)?,
        task_type: row.get(3)?,
        quality: row.get(4)?,
        pass_rate: row.get(5)?,
        avg_cost_usd: row.get(6)?,
        p50_latency_ms: row.get(7)?,
        p95_latency_ms: row.get(8)?,
        n_runs: row.get(9)?,
        n_cases: row.get(10)?,
        received_at: row.get(11)?,
        quality_variance: row.get(12)?,
        judge_provider: row.get(13)?,
        rubric_fingerprint: row.get(14)?,
        determinism: row.get(15)?,
        frozen_dataset: row.get(16)?,
        significance_tested: row.get(17)?,
    })
}

fn cov(tag: Option<String>) -> Coverage {
    tag.as_deref()
        .map(Coverage::from_tag)
        .unwrap_or(Coverage::Unknown)
}

fn from_raw(r: Raw) -> Result<CollectiveEntry> {
    Ok(CollectiveEntry {
        contributor_id: r.contributor_id,
        provider: r.provider,
        model: r.model,
        task_type: r.task_type,
        quality: r.quality,
        pass_rate: r.pass_rate,
        avg_cost_usd: r.avg_cost_usd,
        p50_latency_ms: r.p50_latency_ms.map(|v| v as u64),
        p95_latency_ms: r.p95_latency_ms.map(|v| v as u64),
        n_runs: r.n_runs as u32,
        n_cases: r.n_cases as u32,
        quality_variance: r.quality_variance,
        judge_provider: r.judge_provider,
        rubric_fingerprint: r.rubric_fingerprint,
        determinism: r.determinism,
        // A v1/v2 row stored NULL; `Coverage::from_tag` reads that back as `Unknown`, so the version
        // bump needs no backfill.
        frozen_dataset: cov(r.frozen_dataset),
        significance_tested: cov(r.significance_tested),
        received_at: parse_ts(&r.received_at)?,
    })
}

#[cfg(test)]
mod cols_tests {
    use super::*;

    #[test]
    fn cols_match_the_schema_model() {
        use crate::schema::{tables, Dialect};
        assert_eq!(
            COLS,
            tables::COLLECTIVE_ENTRIES.select_list(Dialect::Sqlite)
        );
    }
}
