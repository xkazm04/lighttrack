//! Cloud→device relay queue (docs/RELAY.md).
//!
//! Same semantics as the SQLite reference (`lighttrack-store/src/sqlite/relay.rs`), adapted for a
//! pooled, concurrent backend: `lease`/`sweep_dead` use `FOR UPDATE SKIP LOCKED` / single-statement
//! `UPDATE … RETURNING` so parallel callers can't double-lease, and `settle` wraps its
//! read-branch-update in one transaction with `SELECT … FOR UPDATE` so a duplicate result report
//! observes the settled row instead of double-applying (the API's run-event logging relies on that).

use chrono::{Duration, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{RelayStatus, RelayTask, RELAY_ERROR_DEVICE_LOST, RELAY_MAX_STALE_RECLAIMS};
use lighttrack_store::Result;

use crate::devices::CapabilityFilter;
use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

pub(crate) const COLS: &str =
    "id, project_id, source, action_type, payload, status, attempts, max_attempts, \
    retry_interval_secs, idempotency_key, device, lease_deadline, next_attempt_at, result, error, \
    created_at, updated_at, failures, stale_reclaims, lease_fence, progress";

pub(crate) async fn create(pool: &PgPool, t: &RelayTask) -> Result<()> {
    let payload = json_or_null(&t.payload)?;
    let result = json_or_null(&t.result)?;
    sqlx::query(&format!(
        "INSERT INTO relay_tasks ({COLS}) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)"
    ))
    .bind(t.id.clone())
    .bind(t.project_id.clone())
    .bind(t.source.clone())
    .bind(t.action_type.clone())
    .bind(payload)
    .bind(t.status.clone())
    .bind(t.attempts as i64)
    .bind(t.max_attempts as i64)
    .bind(t.retry_interval_secs as i64)
    .bind(t.idempotency_key.clone())
    .bind(t.device.clone())
    .bind(t.lease_deadline.map(fmt_ts))
    .bind(fmt_ts(t.next_attempt_at))
    .bind(result)
    .bind(t.error.clone())
    .bind(fmt_ts(t.created_at))
    .bind(fmt_ts(t.updated_at))
    .bind(t.failures as i64)
    .bind(t.stale_reclaims as i64)
    .bind(t.lease_fence.map(fmt_ts))
    .bind(t.progress.clone())
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
) -> Result<Option<RelayTask>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM relay_tasks WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)"
    ))
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn find_by_key(
    pool: &PgPool,
    project: &str,
    key: &str,
) -> Result<Option<RelayTask>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM relay_tasks WHERE project_id = $1 AND idempotency_key = $2"
    ))
    .bind(project.to_string())
    .bind(key.to_string())
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn list(
    pool: &PgPool,
    project: Option<&str>,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<RelayTask>> {
    list_where(pool, project, None, status, limit).await
}

/// [`list`] narrowed to one `action_type` (M19).
pub(crate) async fn list_by_action(
    pool: &PgPool,
    project: Option<&str>,
    action_type: &str,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<RelayTask>> {
    list_where(pool, project, Some(action_type), status, limit).await
}

async fn list_where(
    pool: &PgPool,
    project: Option<&str>,
    action_type: Option<&str>,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<RelayTask>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM relay_tasks \
         WHERE ($1::text IS NULL OR project_id = $1) AND ($2::text IS NULL OR status = $2) \
           AND ($4::text IS NULL OR action_type = $4) \
         ORDER BY created_at DESC LIMIT $3"
    ))
    .bind(project.map(str::to_string))
    .bind(status.map(str::to_string))
    .bind(limit as i64)
    .bind(action_type.map(str::to_string))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// Dead-letter expired leases with no budget left. Two independent budgets end a task here — the
/// retry budget (`failures`) and the device-death budget (`stale_reclaims`) — because a task that
/// reliably kills its device never reports a failure, so one counter would re-lease it forever.
/// `cancelling` is untouched: a cancelled task ends as `cancelled`, never as `dead`.
pub(crate) async fn sweep_dead(pool: &PgPool) -> Result<Vec<RelayTask>> {
    let now = fmt_ts(Utc::now());
    let rows = sqlx::query(&format!(
        "UPDATE relay_tasks SET status='dead', \
             error=COALESCE(error, $2), lease_deadline=NULL, lease_fence=NULL, updated_at=$1 \
         WHERE status='leased' AND lease_deadline < $1 \
           AND (failures >= max_attempts OR stale_reclaims >= $3) \
         RETURNING {COLS}"
    ))
    .bind(now)
    .bind(RELAY_ERROR_DEVICE_LOST)
    .bind(RELAY_MAX_STALE_RECLAIMS as i64)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn lease(
    pool: &PgPool,
    device: &str,
    capabilities: &[String],
    lease_secs: i64,
    max: usize,
) -> Result<Vec<RelayTask>> {
    let now = Utc::now();
    let now_s = fmt_ts(now);
    let deadline = fmt_ts(now + Duration::seconds(lease_secs.max(0)));
    // The capability narrowing goes INSIDE the sub-select, beside the due/expired predicates and
    // the `FOR UPDATE SKIP LOCKED`: it is part of what makes a task leasable *by this device*, not
    // a filter on rows already claimed. Applied outside, the `UPDATE` would still have stamped its
    // fence on the tasks it then dropped.
    let caps = CapabilityFilter::build(capabilities, 7);
    let sql = format!(
        "UPDATE relay_tasks SET status='leased', device=$1, lease_deadline=$2, lease_fence=$3, \
             attempts=attempts+1, progress=NULL, updated_at=$3, \
             stale_reclaims = stale_reclaims + (CASE WHEN status='leased' THEN 1 ELSE 0 END), \
             error = CASE WHEN status='leased' THEN $5 ELSE error END \
         WHERE id IN (SELECT id FROM relay_tasks \
                      WHERE ((status='queued' AND next_attempt_at <= $3) \
                         OR (status='leased' AND lease_deadline < $3 \
                             AND failures < max_attempts AND stale_reclaims < $6)) \
                        AND {} \
                      ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT $4) \
         RETURNING {COLS}",
        caps.clause
    );
    let mut q = sqlx::query(&sql)
        .bind(device.to_string())
        .bind(deadline)
        .bind(now_s)
        .bind(max as i64)
        .bind(RELAY_ERROR_DEVICE_LOST)
        .bind(RELAY_MAX_STALE_RECLAIMS as i64);
    for v in &caps.values {
        q = q.bind(v.clone());
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// Whether `status` is a state a settle/renew/progress may still be accepted in. `cancelling` is
/// inside it on purpose: a device asked to stop is still running and still has to report honestly.
pub(crate) fn is_live_lease(status: &str) -> bool {
    status == RelayStatus::Leased.as_str() || status == RelayStatus::Cancelling.as_str()
}

pub(crate) fn from_row(row: &PgRow) -> Result<RelayTask> {
    let payload: Option<String> = row.try_get(4).map_err(pgerr)?;
    let lease_deadline: Option<String> = row.try_get(11).map_err(pgerr)?;
    let next_attempt_at: String = row.try_get(12).map_err(pgerr)?;
    let result: Option<String> = row.try_get(13).map_err(pgerr)?;
    let created_at: String = row.try_get(15).map_err(pgerr)?;
    let updated_at: String = row.try_get(16).map_err(pgerr)?;
    let lease_fence: Option<String> = row.try_get(19).map_err(pgerr)?;
    Ok(RelayTask {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        source: row.try_get(2).map_err(pgerr)?,
        action_type: row.try_get(3).map_err(pgerr)?,
        payload: val_or_null(payload)?,
        status: row.try_get(5).map_err(pgerr)?,
        attempts: row.try_get::<i64, _>(6).map_err(pgerr)? as u32,
        max_attempts: row.try_get::<i64, _>(7).map_err(pgerr)? as u32,
        retry_interval_secs: row.try_get::<i64, _>(8).map_err(pgerr)? as u32,
        idempotency_key: row.try_get(9).map_err(pgerr)?,
        device: row.try_get(10).map_err(pgerr)?,
        lease_deadline: match lease_deadline {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        next_attempt_at: parse_ts(&next_attempt_at)?,
        result: val_or_null(result)?,
        error: row.try_get(14).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
        failures: row.try_get::<i64, _>(17).map_err(pgerr)? as u32,
        stale_reclaims: row.try_get::<i64, _>(18).map_err(pgerr)? as u32,
        lease_fence: match lease_fence {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        progress: row.try_get(20).map_err(pgerr)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cols_match_the_schema_model() {
        use lighttrack_store::schema::{tables, Dialect};
        assert_eq!(COLS, tables::RELAY_TASKS.select_list(Dialect::Postgres));
    }
}
