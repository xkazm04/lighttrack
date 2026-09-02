//! Stored schedules: the recurring-workload table the API's schedule sweep walks.
//!
//! Mirrors `lighttrack-store/src/sqlite/schedules.rs`. `enabled` is a real `BOOLEAN` here rather
//! than SQLite's integer, which is the only difference the row mapper has to know about.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::Schedule;
use lighttrack_store::Result;

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

const COLS: &str = "id, project_id, kind, payload, interval_secs, next_due, last_job_id, \
    enabled, created_at";

pub(crate) async fn create(pool: &PgPool, s: &Schedule) -> Result<()> {
    sqlx::query(&format!(
        "INSERT INTO schedules ({COLS}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)"
    ))
    .bind(s.id.clone())
    .bind(s.project_id.clone())
    .bind(s.kind.clone())
    .bind(json_or_null(&s.payload)?)
    .bind(s.interval_secs as i64)
    .bind(fmt_ts(s.next_due))
    .bind(s.last_job_id.clone())
    .bind(s.enabled)
    .bind(fmt_ts(s.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Schedule>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM schedules WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool, project: &str) -> Result<Vec<Schedule>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM schedules WHERE project_id = $1 ORDER BY created_at"
    ))
    .bind(project.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// Full replace of the mutable fields. The id and `project_id` are identity, not state — a schedule
/// that could move between projects would be a way around project scoping — so they are the
/// predicate, never the payload.
pub(crate) async fn update(pool: &PgPool, s: &Schedule) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE schedules SET kind=$2, payload=$3, interval_secs=$4, next_due=$5, \
             last_job_id=$6, enabled=$7 WHERE id=$1",
    )
    .bind(s.id.clone())
    .bind(s.kind.clone())
    .bind(json_or_null(&s.payload)?)
    .bind(s.interval_secs as i64)
    .bind(fmt_ts(s.next_due))
    .bind(s.last_job_id.clone())
    .bind(s.enabled)
    .execute(pool)
    .await
    .map_err(pgerr)?
    .rows_affected();
    Ok(n > 0)
}

pub(crate) async fn delete(pool: &PgPool, id: &str) -> Result<bool> {
    let n = sqlx::query("DELETE FROM schedules WHERE id = $1")
        .bind(id.to_string())
        .execute(pool)
        .await
        .map_err(pgerr)?
        .rows_affected();
    Ok(n > 0)
}

/// Enabled schedules whose `next_due` has passed — the sweep's one read per tick. Disabled rows are
/// excluded in SQL, not filtered afterwards, so `idx_schedules_due` carries the whole predicate.
pub(crate) async fn due(pool: &PgPool, now: DateTime<Utc>) -> Result<Vec<Schedule>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM schedules WHERE enabled AND next_due <= $1 ORDER BY next_due"
    ))
    .bind(fmt_ts(now))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<Schedule> {
    let payload: Option<String> = row.try_get(3).map_err(pgerr)?;
    let next_due: String = row.try_get(5).map_err(pgerr)?;
    let created_at: String = row.try_get(8).map_err(pgerr)?;
    Ok(Schedule {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        kind: row.try_get(2).map_err(pgerr)?,
        payload: val_or_null(payload)?,
        interval_secs: row.try_get::<i64, _>(4).map_err(pgerr)? as u32,
        next_due: parse_ts(&next_due)?,
        last_job_id: row.try_get(6).map_err(pgerr)?,
        enabled: row.try_get(7).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}
