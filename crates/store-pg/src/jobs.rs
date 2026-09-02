//! Background job queue.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Job, JobCancel, JobFinish, JOB_ERROR_WORKER_LOST};
use lighttrack_store::Result;

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

const COLS: &str = "id, type, payload, status, attempts, max_attempts, progress, error, \
    result, claimed_at, created_at, updated_at, failures, stale_reclaims, project_id";

pub(crate) async fn create(pool: &PgPool, j: &Job) -> Result<()> {
    let payload = json_or_null(&j.payload)?;
    let result = json_or_null(&j.result)?;
    sqlx::query(
        "INSERT INTO jobs (id, type, payload, status, attempts, max_attempts, progress, \
         error, result, claimed_at, created_at, updated_at, failures, stale_reclaims, \
         project_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
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
    .bind(j.failures as i64)
    .bind(j.stale_reclaims as i64)
    .bind(j.project_id.clone())
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

/// Claim the oldest claimable job **this worker can actually run**. `kinds` is the worker's
/// capability declaration; empty = "any kind" (what an older runner still sends). The filter is
/// inside the claim because a worker that claims a kind it cannot execute has already taken the job
/// off the queue and stamped a lease on it.
pub(crate) async fn claim(
    pool: &PgPool,
    stale_before: DateTime<Utc>,
    kinds: &[&str],
) -> Result<Option<Job>> {
    let now = fmt_ts(Utc::now());
    let stale = fmt_ts(stale_before);
    // Atomic + concurrency-safe: FOR UPDATE SKIP LOCKED so parallel workers don't grab the same job.
    // Same accounting contract as SQLite: reclaiming a stale `running` job is a WORKER DEATH — it
    // counts in `stale_reclaims` and stamps the marker error, never in `failures` (the retry
    // budget). `cancelling`/`cancelled` sit outside the matched set, so the reclaim path can never
    // restart a run someone cancelled.
    let sql = format!(
        "UPDATE jobs SET status='running', claimed_at=$1, updated_at=$1, attempts=attempts+1, \
                stale_reclaims = stale_reclaims + (CASE WHEN status='running' THEN 1 ELSE 0 END), \
                error = CASE WHEN status='running' THEN $3 ELSE error END \
         WHERE id = (SELECT id FROM jobs \
                     WHERE (status='queued' OR (status='running' AND claimed_at < $2)) \
                       AND ($4::text[] IS NULL OR type = ANY($4)) \
                     ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1) \
         RETURNING {COLS}"
    );
    // `= ANY($4)` with a nullable array keeps this ONE statement at a fixed parameter count; an
    // `IN (…)` list built per call would make the SQL vary with the caller and defeat the prepared
    // statement cache.
    let kinds_arr: Option<Vec<String>> =
        (!kinds.is_empty()).then(|| kinds.iter().map(|k| k.to_string()).collect());
    let row = sqlx::query(&sql)
        .bind(now)
        .bind(stale)
        .bind(JOB_ERROR_WORKER_LOST)
        .bind(kinds_arr)
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

/// Ask a job to stop, in ONE conditional statement so it cannot race a concurrent claim: `queued` →
/// `cancelled`, `running` → `cancelling` (which is not claimable), terminal → untouched.
pub(crate) async fn cancel(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
) -> Result<Option<JobCancel>> {
    let new_status: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET status = CASE WHEN status='queued' THEN 'cancelled' ELSE 'cancelling' END, \
         updated_at = $2 \
         WHERE id = $1 AND status IN ('queued','running') \
           AND ($3::text IS NULL OR project_id = $3) \
         RETURNING status",
    )
    .bind(id.to_string())
    .bind(fmt_ts(Utc::now()))
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    match new_status.as_deref() {
        Some("cancelled") => return Ok(Some(JobCancel::Cancelled)),
        Some(_) => return Ok(Some(JobCancel::Cancelling)),
        None => {}
    }
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT status FROM jobs WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)",
    )
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    Ok(existing.map(|status| JobCancel::AlreadyFinished { status }))
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

/// Extend the holder's lease: one conditioned statement moving `claimed_at` forward only where it
/// is still `fence` and the job is still live. Zero rows means this caller no longer holds the job —
/// affirmative evidence its work loop must read and stop on, not a guess.
///
/// `cancelling` is renewable on purpose: a run being asked to stop is still running, still spending,
/// and still has to reach its next case boundary. Dropping its lease would let the reclaim path
/// start a second copy of a run that is already winding down.
pub(crate) async fn renew_lease(
    pool: &PgPool,
    id: &str,
    fence: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    let updated: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET claimed_at = $3, updated_at = $3 \
         WHERE id = $1 AND claimed_at = $2 AND status IN ('running','cancelling') \
         RETURNING claimed_at",
    )
    .bind(id.to_string())
    .bind(fmt_ts(fence))
    .bind(fmt_ts(now))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    updated.map(|s| parse_ts(&s)).transpose()
}

/// Finish a job — the last write in the lifecycle, and a conditioned one like every other.
///
/// Two conditions: **still non-terminal** (a verdict is final) and, when a `fence` is supplied,
/// **still mine** (`claimed_at` is exactly what the caller was handed at claim). Without the second,
/// a worker reclaimed as stale while it was busy finishes later and overwrites the verdict its
/// replacement already wrote — silently, with a plausible-looking result.
pub(crate) async fn finish(
    pool: &PgPool,
    id: &str,
    status: &str,
    result: &Value,
    error: Option<&str>,
    fence: Option<DateTime<Utc>>,
) -> Result<JobFinish> {
    let result_s = json_or_null(result)?;
    // An error means the job RAN and the work failed → it consumes the retry budget (`failures`).
    // A clean finish, including a cancellation, never does.
    let landed: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET status = $2, result = $3, error = $4, updated_at = $5, \
                failures = failures + (CASE WHEN $4::text IS NULL THEN 0 ELSE 1 END) \
         WHERE id = $1 \
           AND status NOT IN ('done','failed','cancelled') \
           AND ($6::text IS NULL OR claimed_at = $6) \
         RETURNING id",
    )
    .bind(id.to_string())
    .bind(status.to_string())
    .bind(result_s)
    .bind(error.map(str::to_string))
    .bind(fmt_ts(Utc::now()))
    .bind(fence.map(fmt_ts))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    if landed.is_some() {
        return Ok(JobFinish::Finished);
    }
    // Refused: report what the record actually holds now, so the loser can name what beat it
    // instead of reporting a bare failure.
    Ok(match get(pool, None, id).await? {
        Some(j) => JobFinish::NotHeld {
            status: j.status,
            claimed_at: j.claimed_at,
        },
        None => JobFinish::NoSuchJob,
    })
}

pub(crate) async fn get(pool: &PgPool, project: Option<&str>, id: &str) -> Result<Option<Job>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM jobs WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)"
    ))
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

/// The queue as one scope sees it. A project reads only the work stamped with its own id; the
/// operator additionally reads the project-less rows (sweeps, and anything enqueued before the
/// column existed).
pub(crate) async fn list(
    pool: &PgPool,
    project: Option<&str>,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<Job>> {
    let rows = match status {
        Some(s) => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM jobs \
                 WHERE status = $1 AND ($3::text IS NULL OR project_id = $3) \
                 ORDER BY created_at DESC LIMIT $2"
            ))
            .bind(s.to_string())
            .bind(limit as i64)
            .bind(project.map(str::to_string))
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM jobs \
                 WHERE ($2::text IS NULL OR project_id = $2) \
                 ORDER BY created_at DESC LIMIT $1"
            ))
            .bind(limit as i64)
            .bind(project.map(str::to_string))
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
        failures: row.try_get::<i64, _>(12).map_err(pgerr)? as u32,
        stale_reclaims: row.try_get::<i64, _>(13).map_err(pgerr)? as u32,
        project_id: row.try_get(14).map_err(pgerr)?,
    })
}
