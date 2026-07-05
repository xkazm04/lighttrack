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

use lighttrack_core::{RelayOutcome, RelayTask};
use lighttrack_store::Result;

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

const COLS: &str = "id, project_id, source, action_type, payload, status, attempts, max_attempts, \
    retry_interval_secs, idempotency_key, device, lease_deadline, next_attempt_at, result, error, \
    created_at, updated_at";

pub(crate) async fn create(pool: &PgPool, t: &RelayTask) -> Result<()> {
    let payload = json_or_null(&t.payload)?;
    let result = json_or_null(&t.result)?;
    sqlx::query(&format!(
        "INSERT INTO relay_tasks ({COLS}) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)"
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
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<RelayTask>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM relay_tasks WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn find_by_key(pool: &PgPool, project: &str, key: &str) -> Result<Option<RelayTask>> {
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
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM relay_tasks \
         WHERE ($1::text IS NULL OR project_id = $1) AND ($2::text IS NULL OR status = $2) \
         ORDER BY created_at DESC LIMIT $3"
    ))
    .bind(project.map(str::to_string))
    .bind(status.map(str::to_string))
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn sweep_dead(pool: &PgPool) -> Result<Vec<RelayTask>> {
    let now = fmt_ts(Utc::now());
    let rows = sqlx::query(&format!(
        "UPDATE relay_tasks SET status='dead', \
             error=COALESCE(error, 'lease expired without a result'), updated_at=$1 \
         WHERE status='leased' AND lease_deadline < $1 AND attempts >= max_attempts \
         RETURNING {COLS}"
    ))
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn lease(
    pool: &PgPool,
    device: &str,
    lease_secs: i64,
    max: usize,
) -> Result<Vec<RelayTask>> {
    let now = Utc::now();
    let now_s = fmt_ts(now);
    let deadline = fmt_ts(now + Duration::seconds(lease_secs.max(0)));
    let rows = sqlx::query(&format!(
        "UPDATE relay_tasks SET status='leased', device=$1, lease_deadline=$2, \
             attempts=attempts+1, updated_at=$3 \
         WHERE id IN (SELECT id FROM relay_tasks \
                      WHERE (status='queued' AND next_attempt_at <= $3) \
                         OR (status='leased' AND lease_deadline < $3 AND attempts < max_attempts) \
                      ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT $4) \
         RETURNING {COLS}"
    ))
    .bind(device.to_string())
    .bind(deadline)
    .bind(now_s)
    .bind(max as i64)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn settle(
    pool: &PgPool,
    id: &str,
    outcome: &RelayOutcome,
) -> Result<Option<RelayTask>> {
    let mut tx = pool.begin().await.map_err(pgerr)?;
    let row = sqlx::query(&format!("SELECT {COLS} FROM relay_tasks WHERE id = $1 FOR UPDATE"))
        .bind(id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(pgerr)?;
    let Some(task) = row.as_ref().map(from_row).transpose()? else {
        return Ok(None); // tx rolls back on drop
    };
    if task.status != "leased" {
        return Ok(Some(task)); // duplicate report: settled row returned unchanged
    }
    let now = Utc::now();
    let now_s = fmt_ts(now);
    match outcome {
        RelayOutcome::Succeeded(result) => {
            let result_s = json_or_null(result)?;
            sqlx::query(
                "UPDATE relay_tasks SET status='succeeded', result=$2, error=NULL, \
                     lease_deadline=NULL, updated_at=$3 WHERE id=$1",
            )
            .bind(id.to_string())
            .bind(result_s)
            .bind(now_s)
            .execute(&mut *tx)
            .await
            .map_err(pgerr)?;
        }
        RelayOutcome::Failed(err) => {
            let (status, next) = if task.attempts >= task.max_attempts {
                ("dead", task.next_attempt_at)
            } else {
                ("queued", now + Duration::seconds(task.retry_interval_secs as i64))
            };
            sqlx::query(
                "UPDATE relay_tasks SET status=$2, error=$3, next_attempt_at=$4, \
                     lease_deadline=NULL, updated_at=$5 WHERE id=$1",
            )
            .bind(id.to_string())
            .bind(status.to_string())
            .bind(err.clone())
            .bind(fmt_ts(next))
            .bind(now_s)
            .execute(&mut *tx)
            .await
            .map_err(pgerr)?;
        }
        RelayOutcome::Deferred { retry_after_secs, reason } => {
            // Not the task's fault (e.g. subscription window exhausted): hand the attempt back.
            let attempts = task.attempts.saturating_sub(1);
            let delay = retry_after_secs.unwrap_or(task.retry_interval_secs);
            let next = now + Duration::seconds(delay as i64);
            sqlx::query(
                "UPDATE relay_tasks SET status='queued', attempts=$2, error=$3, \
                     next_attempt_at=$4, lease_deadline=NULL, updated_at=$5 WHERE id=$1",
            )
            .bind(id.to_string())
            .bind(attempts as i64)
            .bind(reason.clone().or(task.error.clone()))
            .bind(fmt_ts(next))
            .bind(now_s)
            .execute(&mut *tx)
            .await
            .map_err(pgerr)?;
        }
    }
    let updated = sqlx::query(&format!("SELECT {COLS} FROM relay_tasks WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(pgerr)?;
    let updated = updated.as_ref().map(from_row).transpose()?;
    tx.commit().await.map_err(pgerr)?;
    Ok(updated)
}

fn from_row(row: &PgRow) -> Result<RelayTask> {
    let payload: Option<String> = row.try_get(4).map_err(pgerr)?;
    let lease_deadline: Option<String> = row.try_get(11).map_err(pgerr)?;
    let next_attempt_at: String = row.try_get(12).map_err(pgerr)?;
    let result: Option<String> = row.try_get(13).map_err(pgerr)?;
    let created_at: String = row.try_get(15).map_err(pgerr)?;
    let updated_at: String = row.try_get(16).map_err(pgerr)?;
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
    })
}
