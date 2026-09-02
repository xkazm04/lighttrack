//! Stored schedules: the recurring-workload table the API's schedule sweep walks.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::Schedule;

use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

const COLS: &str = "id, project_id, kind, payload, interval_secs, next_due, last_job_id, \
    enabled, created_at";

pub(super) fn create(conn: &Connection, s: &Schedule) -> Result<()> {
    conn.execute(
        &format!("INSERT INTO schedules ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"),
        params![
            s.id,
            s.project_id,
            s.kind,
            json_or_null(&s.payload)?,
            s.interval_secs as i64,
            fmt_ts(s.next_due),
            s.last_job_id,
            s.enabled as i64,
            fmt_ts(s.created_at),
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Schedule>> {
    let sql = format!("SELECT {COLS} FROM schedules WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn list(conn: &Connection, project: &str) -> Result<Vec<Schedule>> {
    let sql = format!("SELECT {COLS} FROM schedules WHERE project_id = ?1 ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Full replace of the mutable fields. The id and `project_id` are identity, not state — a schedule
/// that could move between projects would be a way around project scoping — so they are the
/// predicate, never the payload.
pub(super) fn update(conn: &Connection, s: &Schedule) -> Result<bool> {
    let n = conn.execute(
        "UPDATE schedules SET kind=?2, payload=?3, interval_secs=?4, next_due=?5, \
             last_job_id=?6, enabled=?7 WHERE id=?1",
        params![
            s.id,
            s.kind,
            json_or_null(&s.payload)?,
            s.interval_secs as i64,
            fmt_ts(s.next_due),
            s.last_job_id,
            s.enabled as i64,
        ],
    )?;
    Ok(n > 0)
}

pub(super) fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// Enabled schedules whose `next_due` has passed. Disabled rows are excluded in SQL rather than
/// filtered afterwards: `idx_schedules_due` is keyed on `(enabled, next_due)`, so a deployment that
/// parks a hundred schedules still costs one index range scan per sweep.
pub(super) fn due(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<Schedule>> {
    let sql = format!(
        "SELECT {COLS} FROM schedules WHERE enabled = 1 AND next_due <= ?1 ORDER BY next_due"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![fmt_ts(now)], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

type ScheduleRaw = (
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    Option<String>,
    i64,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<ScheduleRaw> {
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
    ))
}

fn from_raw(r: ScheduleRaw) -> Result<Schedule> {
    Ok(Schedule {
        id: r.0,
        project_id: r.1,
        kind: r.2,
        payload: val_or_null(r.3)?,
        interval_secs: r.4 as u32,
        next_due: parse_ts(&r.5)?,
        last_job_id: r.6,
        enabled: r.7 != 0,
        created_at: parse_ts(&r.8)?,
    })
}
