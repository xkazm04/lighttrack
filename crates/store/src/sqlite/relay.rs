//! Cloud→device relay queue (docs/RELAY.md): the row, the reads, and the lease/sweep writes.
//!
//! The conditioned writes a *holder* makes — settle, renew, progress — and the operator's cancel
//! live in [`super::relay_lease`], because they share one rule this module does not: every one of
//! them is refused unless the caller still holds the fence stamped at lease time.

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{RelayStatus, RelayTask, RELAY_ERROR_DEVICE_LOST, RELAY_MAX_STALE_RECLAIMS};

use super::devices::CapabilityFilter;
use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

pub(super) const COLS: &str =
    "id, project_id, source, action_type, payload, status, attempts, max_attempts, \
    retry_interval_secs, idempotency_key, device, lease_deadline, next_attempt_at, result, error, \
    created_at, updated_at, failures, stale_reclaims, lease_fence, progress";

pub(super) fn create(conn: &Connection, t: &RelayTask) -> Result<()> {
    let payload = json_or_null(&t.payload)?;
    let result = json_or_null(&t.result)?;
    conn.execute(
        &format!(
            "INSERT INTO relay_tasks ({COLS}) VALUES \
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)"
        ),
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
            t.failures as i64,
            t.stale_reclaims as i64,
            t.lease_fence.map(fmt_ts),
            t.progress,
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

pub(super) fn find_by_key(
    conn: &Connection,
    project: &str,
    key: &str,
) -> Result<Option<RelayTask>> {
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

/// Dead-letter expired leases with no budget left, returning the newly-dead tasks so the caller can
/// alert on them. Two independent budgets end a task here, and they are checked separately on
/// purpose (mirroring the job queue): `failures >= max_attempts` means the action keeps failing,
/// while `stale_reclaims >= RELAY_MAX_STALE_RECLAIMS` means devices keep dying on it — a task that
/// reliably kills its device reports no failures at all, so a single counter would loop forever.
///
/// A task this misses stays `leased` and harmless until the next sweep. `cancelling` is untouched:
/// a cancelled task ends as `cancelled`, never as `dead`.
pub(super) fn sweep_dead(conn: &Connection) -> Result<Vec<RelayTask>> {
    let now_s = fmt_ts(Utc::now());
    let sql = format!(
        "UPDATE relay_tasks SET status='dead', \
             error=COALESCE(error, ?2), lease_deadline=NULL, lease_fence=NULL, updated_at=?1 \
         WHERE status='leased' AND lease_deadline < ?1 \
           AND (failures >= max_attempts OR stale_reclaims >= ?3) \
         RETURNING {COLS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(
            params![
                now_s,
                RELAY_ERROR_DEVICE_LOST,
                RELAY_MAX_STALE_RECLAIMS as i64
            ],
            map_raw,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Lease up to `max` due tasks for `device`: due queued tasks, plus expired leases that still have
/// budget on both counters. Expired-and-exhausted tasks are not touched here — [`sweep_dead`]
/// dead-letters them (the API runs it right before leasing).
///
/// The lease stamps `lease_fence` — the identity every subsequent write by this holder is compared
/// against — and clears the previous holder's `progress`. Reclaiming an expired lease counts a
/// DEVICE DEATH in `stale_reclaims` and stamps the marker error, never a `failures`: the device
/// never reported anything, so nothing is known to have failed. `cancelling`/`cancelled` sit
/// outside the matched set, so a cancelled task is never handed to a second device.
pub(super) fn lease(
    conn: &Connection,
    device: &str,
    capabilities: &[String],
    lease_secs: i64,
    max: usize,
) -> Result<Vec<RelayTask>> {
    let now = Utc::now();
    let now_s = fmt_ts(now);
    let deadline = fmt_ts(now + Duration::seconds(lease_secs.max(0)));
    // The capability narrowing goes INSIDE the sub-select, beside the due/expired predicates: it is
    // part of what makes a task leasable *by this device*, not a filter on rows already claimed.
    // Applied outside, the `UPDATE` would still have stamped its fence on the tasks it then dropped
    // — a device would silently consume claims on work it cannot run.
    let caps = CapabilityFilter::build(capabilities, 7);
    let sql = format!(
        "UPDATE relay_tasks SET status='leased', device=?1, lease_deadline=?2, lease_fence=?3, \
             attempts=attempts+1, progress=NULL, \
             stale_reclaims = stale_reclaims + (status='leased'), \
             error = CASE WHEN status='leased' THEN ?5 ELSE error END, \
             updated_at=?3 \
         WHERE id IN (SELECT id FROM relay_tasks \
                      WHERE ((status='queued' AND next_attempt_at <= ?3) \
                         OR (status='leased' AND lease_deadline < ?3 \
                             AND failures < max_attempts AND stale_reclaims < ?6)) \
                        AND {} \
                      ORDER BY created_at LIMIT ?4) \
         RETURNING {COLS}",
        caps.clause
    );
    let mut stmt = conn.prepare(&sql)?;
    let max = max as i64;
    let reclaims = RELAY_MAX_STALE_RECLAIMS as i64;
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![
        &device,
        &deadline,
        &now_s,
        &max,
        &RELAY_ERROR_DEVICE_LOST,
        &reclaims,
    ];
    args.extend(caps.values.iter().map(|v| v as &dyn rusqlite::ToSql));
    let raws = stmt
        .query_map(args.as_slice(), map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Whether `status` is a state a settle/renew/progress may still be accepted in. `cancelling` is
/// inside it on purpose: a device asked to stop is still running and still has to report honestly.
pub(super) fn is_live_lease(status: &str) -> bool {
    status == RelayStatus::Leased.as_str() || status == RelayStatus::Cancelling.as_str()
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
    (i64, i64, Option<String>, Option<String>),
);

pub(super) fn map_raw(row: &Row) -> rusqlite::Result<RelayRaw> {
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
        // Grouped: rusqlite's tuple `FromRow` impls stop at 16 elements, and the M7 columns are one
        // coherent thing anyway — the lease's own accounting.
        (row.get(17)?, row.get(18)?, row.get(19)?, row.get(20)?),
    ))
}

pub(super) fn from_raw(r: RelayRaw) -> Result<RelayTask> {
    let (failures, stale_reclaims, lease_fence, progress) = r.17;
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
        failures: failures as u32,
        stale_reclaims: stale_reclaims as u32,
        lease_fence: match lease_fence {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        progress,
    })
}
