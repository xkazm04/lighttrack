//! Background job queue.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use lighttrack_core::Job;

use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

const COLS: &str = "id, type, payload, status, attempts, max_attempts, progress, error, \
    result, claimed_at, created_at, updated_at";

pub(super) fn create(conn: &Connection, j: &Job) -> Result<()> {
    let payload = json_or_null(&j.payload)?;
    let result = json_or_null(&j.result)?;
    conn.execute(
        "INSERT INTO jobs \
         (id, type, payload, status, attempts, max_attempts, progress, error, result, claimed_at, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            j.id,
            j.job_type,
            payload,
            j.status,
            j.attempts as i64,
            j.max_attempts as i64,
            j.progress,
            j.error,
            result,
            j.claimed_at.map(fmt_ts),
            fmt_ts(j.created_at),
            fmt_ts(j.updated_at),
        ],
    )?;
    Ok(())
}

pub(super) fn claim(conn: &Connection, stale_before: DateTime<Utc>) -> Result<Option<Job>> {
    let now = fmt_ts(Utc::now());
    let stale = fmt_ts(stale_before);
    // Atomic: pick the oldest queued (or stale-running) job and flip it to running.
    let sql = format!(
        "UPDATE jobs SET status='running', claimed_at=?1, updated_at=?1, attempts=attempts+1 \
         WHERE id = (SELECT id FROM jobs \
                     WHERE status='queued' OR (status='running' AND claimed_at < ?2) \
                     ORDER BY created_at LIMIT 1) \
         RETURNING {COLS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![now, stale], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn update_progress(conn: &Connection, id: &str, progress: &str) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET progress = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, progress, fmt_ts(Utc::now())],
    )?;
    Ok(())
}

pub(super) fn finish(conn: &Connection, id: &str, status: &str, result: &Value, error: Option<&str>) -> Result<()> {
    let result_s = json_or_null(result)?;
    conn.execute(
        "UPDATE jobs SET status = ?2, result = ?3, error = ?4, updated_at = ?5 WHERE id = ?1",
        params![id, status, result_s, error, fmt_ts(Utc::now())],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Job>> {
    let sql = format!("SELECT {COLS} FROM jobs WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn list(conn: &Connection, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
    let raws = if let Some(s) = status {
        let sql =
            format!("SELECT {COLS} FROM jobs WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2");
        let mut stmt = conn.prepare(&sql)?;
        let v = stmt
            .query_map(params![s, limit as i64], map_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    } else {
        let sql = format!("SELECT {COLS} FROM jobs ORDER BY created_at DESC LIMIT ?1");
        let mut stmt = conn.prepare(&sql)?;
        let v = stmt
            .query_map(params![limit as i64], map_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    raws.into_iter().map(from_raw).collect()
}

type JobRaw = (
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<JobRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn from_raw(r: JobRaw) -> Result<Job> {
    Ok(Job {
        id: r.0,
        job_type: r.1,
        payload: val_or_null(r.2)?,
        status: r.3,
        attempts: r.4 as u32,
        max_attempts: r.5 as u32,
        progress: r.6,
        error: r.7,
        result: val_or_null(r.8)?,
        claimed_at: match r.9 {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        created_at: parse_ts(&r.10)?,
        updated_at: parse_ts(&r.11)?,
    })
}
