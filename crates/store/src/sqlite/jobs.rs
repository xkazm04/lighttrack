//! Background job queue.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use lighttrack_core::{Job, JobCancel, JobFinish, JOB_ERROR_WORKER_LOST};

use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

const COLS: &str = "id, type, payload, status, attempts, max_attempts, progress, error, \
    result, claimed_at, created_at, updated_at, failures, stale_reclaims";

pub(super) fn create(conn: &Connection, j: &Job) -> Result<()> {
    let payload = json_or_null(&j.payload)?;
    let result = json_or_null(&j.result)?;
    conn.execute(
        "INSERT INTO jobs \
         (id, type, payload, status, attempts, max_attempts, progress, error, result, claimed_at, \
          created_at, updated_at, failures, stale_reclaims) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
            j.failures as i64,
            j.stale_reclaims as i64,
        ],
    )?;
    Ok(())
}

/// Ask for a job to stop. One conditional statement, so it cannot race a concurrent claim into an
/// inconsistent state:
/// - `queued` → `cancelled` outright (nothing ever ran).
/// - `running` → `cancelling`, which is **not** in the claimable set — so the stale-claim reclaim
///   path can never restart a cancelled run, no matter which of the two statements lands first.
/// - anything terminal → untouched, and reported as already finished.
pub(super) fn cancel(conn: &Connection, id: &str) -> Result<Option<JobCancel>> {
    let now = fmt_ts(Utc::now());
    let mut stmt = conn.prepare(
        "UPDATE jobs SET status = CASE WHEN status='queued' THEN 'cancelled' ELSE 'cancelling' END, \
         updated_at = ?2 \
         WHERE id = ?1 AND status IN ('queued','running') \
         RETURNING status",
    )?;
    let new_status: Option<String> = stmt.query_row(params![id, now], |r| r.get(0)).optional()?;
    match new_status.as_deref() {
        Some("cancelled") => return Ok(Some(JobCancel::Cancelled)),
        Some(_) => return Ok(Some(JobCancel::Cancelling)),
        None => {}
    }
    // Nothing to cancel: either it's already terminal, or there is no such job.
    let existing: Option<String> = conn
        .query_row("SELECT status FROM jobs WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(existing.map(|status| JobCancel::AlreadyFinished { status }))
}

/// Claim the oldest claimable job **this worker can actually run**.
///
/// `kinds` is the worker's capability declaration: empty = "anything" (what every worker meant
/// before the queue carried more than one kind, and what an old runner still sends). Filtering in
/// SQL rather than after the claim is the whole point — a worker that claimed a kind it cannot
/// execute has already taken the job off the queue and stamped a lease on it, so the job then fails
/// its way through the retry budget while a capable worker sits idle beside it.
pub(super) fn claim(
    conn: &Connection,
    stale_before: DateTime<Utc>,
    kinds: &[&str],
) -> Result<Option<Job>> {
    let now = fmt_ts(Utc::now());
    let stale = fmt_ts(stale_before);
    // A JSON array + `json_each` keeps this ONE statement with a fixed parameter count: building an
    // `IN (?,?,?)` list would make the SQL vary with the caller and defeat the statement cache.
    let kinds_json = if kinds.is_empty() {
        None
    } else {
        Some(serde_json::to_string(kinds)?)
    };
    // Atomic: pick the oldest queued (or stale-running) job and flip it to running. Still ONE
    // statement — the load-bearing property of this queue.
    //
    // Reclaiming a `running` job means the worker that held it never finished: that is a WORKER
    // DEATH, not a benchmark failure, so it is counted in `stale_reclaims` (never in `failures`,
    // which is the retry budget) and stamped into `error` so the job row says which one happened.
    // `cancelling`/`cancelled` are outside the matched set: a cancelled run is never restarted.
    let sql = format!(
        "UPDATE jobs SET status='running', claimed_at=?1, updated_at=?1, attempts=attempts+1, \
                stale_reclaims = stale_reclaims + (status='running'), \
                error = CASE WHEN status='running' THEN ?3 ELSE error END \
         WHERE id = (SELECT id FROM jobs \
                     WHERE (status='queued' OR (status='running' AND claimed_at < ?2)) \
                       AND (?4 IS NULL OR type IN (SELECT value FROM json_each(?4))) \
                     ORDER BY created_at LIMIT 1) \
         RETURNING {COLS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_row(
            params![now, stale, JOB_ERROR_WORKER_LOST, kinds_json],
            map_raw,
        )
        .optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn update_progress(conn: &Connection, id: &str, progress: &str) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET progress = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, progress, fmt_ts(Utc::now())],
    )?;
    Ok(())
}

/// Extend the holder's lease. One conditioned statement: move `claimed_at` forward only where it is
/// still `fence` and the job is still live. Zero rows means this caller no longer holds the job —
/// the affirmative evidence its work loop needs to stop, rather than a guess.
///
/// `cancelling` is inside the renewable set on purpose: a run being asked to stop is still running,
/// still spending, and still has to reach its next case boundary and finish honestly. Dropping its
/// lease would let the reclaim path start a second copy of a run that is already winding down.
pub(super) fn renew_lease(
    conn: &Connection,
    id: &str,
    fence: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    let n = conn.execute(
        "UPDATE jobs SET claimed_at = ?3, updated_at = ?3 \
         WHERE id = ?1 AND claimed_at = ?2 AND status IN ('running','cancelling')",
        params![id, fmt_ts(fence), fmt_ts(now)],
    )?;
    Ok((n > 0).then_some(now))
}

/// Finish a job — the last write in the lifecycle, and now a conditioned one like every other.
///
/// Two conditions, both load-bearing:
/// * **still non-terminal** — a verdict is final, so nothing rewrites `done`/`failed`/`cancelled`.
/// * **still mine** (when a `fence` is supplied) — `claimed_at` must be exactly what the caller was
///   handed at claim. A worker reclaimed as stale while it was busy would otherwise finish later
///   and overwrite whatever verdict its replacement had already written, silently and plausibly.
///
/// An error means the job RAN and the work failed, so it increments `failures` — the retry budget.
/// A clean finish (including a cancellation, which carries no error) never does, so a
/// crash-and-reclaim cycle can't consume a job's chances without the benchmark ever failing.
pub(super) fn finish(
    conn: &Connection,
    id: &str,
    status: &str,
    result: &Value,
    error: Option<&str>,
    fence: Option<DateTime<Utc>>,
) -> Result<JobFinish> {
    let result_s = json_or_null(result)?;
    let fence_s = fence.map(fmt_ts);
    let n = conn.execute(
        "UPDATE jobs SET status = ?2, result = ?3, error = ?4, updated_at = ?5, \
                failures = failures + (?4 IS NOT NULL) \
         WHERE id = ?1 \
           AND status NOT IN ('done','failed','cancelled') \
           AND (?6 IS NULL OR claimed_at = ?6)",
        params![id, status, result_s, error, fmt_ts(Utc::now()), fence_s],
    )?;
    if n > 0 {
        return Ok(JobFinish::Finished);
    }
    // Refused. Say what the record actually holds now, so the loser can name what beat it instead
    // of reporting a bare failure.
    Ok(match get(conn, id)? {
        Some(j) => JobFinish::NotHeld {
            status: j.status,
            claimed_at: j.claimed_at,
        },
        None => JobFinish::NoSuchJob,
    })
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
    i64,
    i64,
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
        row.get(12)?,
        row.get(13)?,
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
        failures: r.12 as u32,
        stale_reclaims: r.13 as u32,
    })
}
