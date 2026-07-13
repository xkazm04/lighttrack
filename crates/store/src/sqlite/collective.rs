//! Collective Model Intelligence — hub-side storage of contributed, privacy-safe digest entries.
//!
//! Rows are pure aggregate (no text, no project/customer ids); the primary key
//! `(contributor_id, provider, model, task_type)` makes a re-contribution upsert in place, and
//! `delete` lets the ingest handler replace a contributor's whole set so dropped buckets don't linger.

use rusqlite::{params, Connection, Row};

use lighttrack_core::CollectiveEntry;

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

pub(super) fn upsert(conn: &Connection, e: &CollectiveEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO collective_entries \
         (contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd, \
          p50_latency_ms, p95_latency_ms, n_runs, n_cases, quality_variance, received_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) \
         ON CONFLICT(contributor_id, provider, model, task_type) DO UPDATE SET \
           quality=excluded.quality, pass_rate=excluded.pass_rate, avg_cost_usd=excluded.avg_cost_usd, \
           p50_latency_ms=excluded.p50_latency_ms, p95_latency_ms=excluded.p95_latency_ms, \
           n_runs=excluded.n_runs, n_cases=excluded.n_cases, \
           quality_variance=excluded.quality_variance, received_at=excluded.received_at",
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

pub(super) fn list(conn: &Connection) -> Result<Vec<CollectiveEntry>> {
    let sql = "SELECT contributor_id, provider, model, task_type, quality, pass_rate, avg_cost_usd, \
               p50_latency_ms, p95_latency_ms, n_runs, n_cases, quality_variance, received_at \
               FROM collective_entries";
    let mut stmt = conn.prepare(sql)?;
    let raws = stmt
        .query_map([], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
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
        quality_variance: row.get(11)?,
        received_at: row.get(12)?,
    })
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
        received_at: parse_ts(&r.received_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lighttrack_core::{merge_leaderboard, DEFAULT_LOW_CONFIDENCE_CASES};

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../../../schema/sqlite/001_init.sql")).unwrap();
        c
    }

    fn entry(contrib: &str, model: &str, q: f64, cases: u32) -> CollectiveEntry {
        CollectiveEntry {
            contributor_id: contrib.into(),
            provider: "anthropic".into(),
            model: model.into(),
            task_type: "qa".into(),
            quality: q,
            pass_rate: q,
            avg_cost_usd: 0.003,
            p50_latency_ms: Some(900),
            p95_latency_ms: Some(2100),
            n_runs: 1,
            n_cases: cases,
            quality_variance: None,
            received_at: Utc::now(),
        }
    }

    #[test]
    fn upsert_is_idempotent_on_pk() {
        let c = conn();
        upsert(&c, &entry("contrib-a", "haiku", 0.7, 10)).unwrap();
        // Same (contributor, provider, model, task) → updates in place, not a second row.
        upsert(&c, &entry("contrib-a", "haiku", 0.9, 40)).unwrap();
        let all = list(&c).unwrap();
        assert_eq!(all.len(), 1);
        assert!((all[0].quality - 0.9).abs() < 1e-9);
        assert_eq!(all[0].n_cases, 40);
    }

    #[test]
    fn delete_replaces_a_contributors_set() {
        let c = conn();
        upsert(&c, &entry("a", "haiku", 0.7, 10)).unwrap();
        upsert(&c, &entry("a", "sonnet", 0.8, 10)).unwrap();
        upsert(&c, &entry("b", "haiku", 0.6, 10)).unwrap();
        let removed = delete(&c, "a").unwrap();
        assert_eq!(removed, 2);
        let all = list(&c).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].contributor_id, "b");
    }

    #[test]
    fn round_trips_into_a_merged_leaderboard() {
        let c = conn();
        upsert(&c, &entry("a", "sonnet", 0.8, 50)).unwrap();
        upsert(&c, &entry("b", "sonnet", 0.9, 50)).unwrap();
        let rows = merge_leaderboard(&list(&c).unwrap(), DEFAULT_LOW_CONFIDENCE_CASES);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "sonnet");
        assert_eq!(rows[0].n_contributors, 2);
        assert_eq!(rows[0].n_cases, 100);
        assert!((rows[0].quality - 0.85).abs() < 1e-9);
        assert_eq!(rows[0].p50_latency_ms, Some(900));
    }

    #[test]
    fn quality_variance_round_trips() {
        let c = conn();
        let mut e = entry("a", "sonnet", 0.8, 50);
        e.quality_variance = Some(0.0081);
        upsert(&c, &e).unwrap();
        let got = list(&c).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].quality_variance, Some(0.0081));
        // A NULL (v1) variance also round-trips as None.
        upsert(&c, &entry("b", "haiku", 0.7, 20)).unwrap();
        let b = list(&c).unwrap().into_iter().find(|r| r.model == "haiku").unwrap();
        assert!(b.quality_variance.is_none());
    }
}
