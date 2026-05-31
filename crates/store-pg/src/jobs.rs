//! Background job queue.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::Job;
use lighttrack_store::Result;

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

const COLS: &str = "id, type, payload, status, attempts, max_attempts, progress, error, \
    result, claimed_at, created_at, updated_at";

pub(crate) async fn create(pool: &PgPool, j: &Job) -> Result<()> {
    let payload = json_or_null(&j.payload)?;
    let result = json_or_null(&j.result)?;
    sqlx::query(
        "INSERT INTO jobs (id, type, payload, status, attempts, max_attempts, progress, \
         error, result, claimed_at, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(j.id.clone())
    .bind(j.job_type.clone())
    .bind(payload)
    .bind(j.status.clone())
    .bind(j.attempts as i64)
    .bind(j.max_attempts as i64)
    .bind(j.progress.clone())
    .bind(j.error.clone())
    .bind(result)
    .bind(j.claimed_at.map(fmt_ts))
    .bind(fmt_ts(j.created_at))
    .bind(fmt_ts(j.updated_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn claim(pool: &PgPool, stale_before: DateTime<Utc>) -> Result<Option<Job>> {
    let now = fmt_ts(Utc::now());
    let stale = fmt_ts(stale_before);
    // Atomic + concurrency-safe: FOR UPDATE SKIP LOCKED so parallel workers don't grab the same job.
    let sql = format!(
        "UPDATE jobs SET status='running', claimed_at=$1, updated_at=$1, attempts=attempts+1 \
         WHERE id = (SELECT id FROM jobs \
                     WHERE status='queued' OR (status='running' AND claimed_at < $2) \
                     ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1) \
         RETURNING {COLS}"
    );
    let row = sqlx::query(&sql)
        .bind(now)
        .bind(stale)
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn update_progress(pool: &PgPool, id: &str, progress: &str) -> Result<()> {
    sqlx::query("UPDATE jobs SET progress = $2, updated_at = $3 WHERE id = $1")
        .bind(id.to_string())
        .bind(progress.to_string())
        .bind(fmt_ts(Utc::now()))
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn finish(pool: &PgPool, id: &str, status: &str, result: &Value, error: Option<&str>) -> Result<()> {
    let result_s = json_or_null(result)?;
    sqlx::query("UPDATE jobs SET status = $2, result = $3, error = $4, updated_at = $5 WHERE id = $1")
        .bind(id.to_string())
        .bind(status.to_string())
        .bind(result_s)
        .bind(error.map(str::to_string))
        .bind(fmt_ts(Utc::now()))
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Job>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM jobs WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
    let rows = match status {
        Some(s) => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM jobs WHERE status = $1 ORDER BY created_at DESC LIMIT $2"
            ))
            .bind(s.to_string())
            .bind(limit as i64)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(&format!("SELECT {COLS} FROM jobs ORDER BY created_at DESC LIMIT $1"))
                .bind(limit as i64)
                .fetch_all(pool)
                .await
        }
    }
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<Job> {
    let payload: Option<String> = row.try_get(2).map_err(pgerr)?;
    let result: Option<String> = row.try_get(8).map_err(pgerr)?;
    let claimed_at: Option<String> = row.try_get(9).map_err(pgerr)?;
    let created_at: String = row.try_get(10).map_err(pgerr)?;
    let updated_at: String = row.try_get(11).map_err(pgerr)?;
    Ok(Job {
        id: row.try_get(0).map_err(pgerr)?,
        job_type: row.try_get(1).map_err(pgerr)?,
        payload: val_or_null(payload)?,
        status: row.try_get(3).map_err(pgerr)?,
        attempts: row.try_get::<i64, _>(4).map_err(pgerr)? as u32,
        max_attempts: row.try_get::<i64, _>(5).map_err(pgerr)? as u32,
        progress: row.try_get(6).map_err(pgerr)?,
        error: row.try_get(7).map_err(pgerr)?,
        result: val_or_null(result)?,
        claimed_at: match claimed_at {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}
