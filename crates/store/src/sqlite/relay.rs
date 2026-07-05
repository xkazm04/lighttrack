//! Cloud→device relay queue (docs/RELAY.md).

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{RelayOutcome, RelayTask};

use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

const COLS: &str = "id, project_id, source, action_type, payload, status, attempts, max_attempts, \
    retry_interval_secs, idempotency_key, device, lease_deadline, next_attempt_at, result, error, \
    created_at, updated_at";

pub(super) fn create(conn: &Connection, t: &RelayTask) -> Result<()> {
    let payload = json_or_null(&t.payload)?;
    let result = json_or_null(&t.result)?;
    conn.execute(
        &format!("INSERT INTO relay_tasks ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"),
        params![
            t.id,
            t.project_id,
            t.source,
            t.action_type,
            payload,
            t.status,
            t.attempts as i64,
            t.max_attempts as i64,
            t.retry_interval_secs as i64,
            t.idempotency_key,
            t.device,
            t.lease_deadline.map(fmt_ts),
            fmt_ts(t.next_attempt_at),
            result,
            t.error,
            fmt_ts(t.created_at),
            fmt_ts(t.updated_at),
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<RelayTask>> {
    let sql = format!("SELECT {COLS} FROM relay_tasks WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn find_by_key(conn: &Connection, project: &str, key: &str) -> Result<Option<RelayTask>> {
    let sql =
        format!("SELECT {COLS} FROM relay_tasks WHERE project_id = ?1 AND idempotency_key = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![project, key], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn list(
    conn: &Connection,
    project: Option<&str>,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<RelayTask>> {
    // Both filters are optional; a NULL parameter disables its clause.
    let sql = format!(
        "SELECT {COLS} FROM relay_tasks \
         WHERE (?1 IS NULL OR project_id = ?1) AND (?2 IS NULL OR status = ?2) \
         ORDER BY created_at DESC LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project, status, limit as i64], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Dead-letter expired leases whose attempts are already exhausted (the device vanished mid-run
/// for the last time), returning the newly-dead tasks so the caller can alert on them. Callers
/// run this before [`lease`]; a task it misses stays `leased` and harmless until the next sweep.
pub(super) fn sweep_dead(conn: &Connection) -> Result<Vec<RelayTask>> {
    let now_s = fmt_ts(Utc::now());
    let sql = format!(
        "UPDATE relay_tasks SET status='dead', \
             error=COALESCE(error, 'lease expired without a result'), updated_at=?1 \
         WHERE status='leased' AND lease_deadline < ?1 AND attempts >= max_attempts \
         RETURNING {COLS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![now_s], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Lease up to `max` due tasks for `device`, consuming an attempt each: due queued tasks, plus
/// expired leases with attempts to spare. Expired-and-exhausted tasks are not touched here —
/// [`sweep_dead`] dead-letters them (the API runs it right before leasing).
pub(super) fn lease(
    conn: &Connection,
    device: &str,
    lease_secs: i64,
    max: usize,
) -> Result<Vec<RelayTask>> {
    let now = Utc::now();
    let now_s = fmt_ts(now);
    let deadline = fmt_ts(now + Duration::seconds(lease_secs.max(0)));
    let sql = format!(
        "UPDATE relay_tasks SET status='leased', device=?1, lease_deadline=?2, \
             attempts=attempts+1, updated_at=?3 \
         WHERE id IN (SELECT id FROM relay_tasks \
                      WHERE (status='queued' AND next_attempt_at <= ?3) \
                         OR (status='leased' AND lease_deadline < ?3 AND attempts < max_attempts) \
                      ORDER BY created_at LIMIT ?4) \
         RETURNING {COLS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![device, deadline, now_s, max as i64], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Settle a leased task; a task that is no longer leased is returned unchanged so duplicate
/// result reports are harmless.
pub(super) fn settle(
    conn: &Connection,
    id: &str,
    outcome: &RelayOutcome,
) -> Result<Option<RelayTask>> {
    let Some(task) = get(conn, id)? else {
        return Ok(None);
    };
    if task.status != "leased" {
        return Ok(Some(task));
    }
    let now = Utc::now();
    let now_s = fmt_ts(now);
    match outcome {
        RelayOutcome::Succeeded(result) => {
            let result_s = json_or_null(result)?;
            conn.execute(
                "UPDATE relay_tasks SET status='succeeded', result=?2, error=NULL, \
                     lease_deadline=NULL, updated_at=?3 WHERE id=?1",
                params![id, result_s, now_s],
            )?;
        }
        RelayOutcome::Failed(err) => {
            let (status, next) = if task.attempts >= task.max_attempts {
                ("dead", task.next_attempt_at)
            } else {
                ("queued", now + Duration::seconds(task.retry_interval_secs as i64))
            };
            conn.execute(
                "UPDATE relay_tasks SET status=?2, error=?3, next_attempt_at=?4, \
                     lease_deadline=NULL, updated_at=?5 WHERE id=?1",
                params![id, status, err, fmt_ts(next), now_s],
            )?;
        }
        RelayOutcome::Deferred { retry_after_secs, reason } => {
            // Not the task's fault (e.g. subscription window exhausted): hand the attempt back.
            let attempts = task.attempts.saturating_sub(1);
            let delay = retry_after_secs.unwrap_or(task.retry_interval_secs);
            let next = now + Duration::seconds(delay as i64);
            conn.execute(
                "UPDATE relay_tasks SET status='queued', attempts=?2, error=?3, \
                     next_attempt_at=?4, lease_deadline=NULL, updated_at=?5 WHERE id=?1",
                params![id, attempts as i64, reason.as_deref().or(task.error.as_deref()), fmt_ts(next), now_s],
            )?;
        }
    }
    get(conn, id)
}

type RelayRaw = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<RelayRaw> {
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
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
    ))
}

fn from_raw(r: RelayRaw) -> Result<RelayTask> {
    Ok(RelayTask {
        id: r.0,
        project_id: r.1,
        source: r.2,
        action_type: r.3,
        payload: val_or_null(r.4)?,
        status: r.5,
        attempts: r.6 as u32,
        max_attempts: r.7 as u32,
        retry_interval_secs: r.8 as u32,
        idempotency_key: r.9,
        device: r.10,
        lease_deadline: match r.11 {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        next_attempt_at: parse_ts(&r.12)?,
        result: val_or_null(r.13)?,
        error: r.14,
        created_at: parse_ts(&r.15)?,
        updated_at: parse_ts(&r.16)?,
    })
}
