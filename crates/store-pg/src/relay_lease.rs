//! The conditioned half of the relay queue: every write a holder makes, plus the operator's cancel.
//!
//! Same semantics as the SQLite reference (`lighttrack-store/src/sqlite/relay_lease.rs`), adapted
//! for a pooled, concurrent backend: `settle` wraps its read-branch-update in one transaction with
//! `SELECT … FOR UPDATE`, so two devices reporting on the same task cannot both observe themselves
//! as the holder. The fence check is what makes the loser's report a refusal instead of a clobber.

use chrono::{DateTime, Duration, Utc};
use sqlx::postgres::PgPool;

use lighttrack_core::{LeaseHeld, RelayCancel, RelayOutcome, RelaySettle, RelayStatus};
use lighttrack_store::Result;

use crate::relay::{from_row, is_live_lease, COLS};
use crate::util::{fmt_ts, json_or_null, pgerr};

/// Extend the holder's lease, conditioned on `fence`. Moves the DEADLINE and never the fence, so
/// one device's lease keeps one identity for its whole run.
pub(crate) async fn renew(
    pool: &PgPool,
    id: &str,
    fence: DateTime<Utc>,
    lease_secs: i64,
) -> Result<LeaseHeld> {
    let now = Utc::now();
    let deadline = now + Duration::seconds(lease_secs.max(0));
    let n = sqlx::query(
        "UPDATE relay_tasks SET lease_deadline = $3, updated_at = $4 \
         WHERE id = $1 AND lease_fence = $2 AND status IN ('leased','cancelling')",
    )
    .bind(id.to_string())
    .bind(fmt_ts(fence))
    .bind(fmt_ts(deadline))
    .bind(fmt_ts(now))
    .execute(pool)
    .await
    .map_err(pgerr)?
    .rows_affected();
    if n > 0 {
        return Ok(LeaseHeld::Held {
            deadline: Some(deadline),
        });
    }
    not_held(pool, id).await
}

/// Publish liveness detail, on its own door — never on the renewal, or a device that is alive but
/// stuck computing something to say reads as a dead one.
pub(crate) async fn update_progress(
    pool: &PgPool,
    id: &str,
    fence: DateTime<Utc>,
    progress: &str,
) -> Result<LeaseHeld> {
    let n = sqlx::query(
        "UPDATE relay_tasks SET progress = $3, updated_at = $4 \
         WHERE id = $1 AND lease_fence = $2 AND status IN ('leased','cancelling')",
    )
    .bind(id.to_string())
    .bind(fmt_ts(fence))
    .bind(progress.to_string())
    .bind(fmt_ts(Utc::now()))
    .execute(pool)
    .await
    .map_err(pgerr)?
    .rows_affected();
    if n > 0 {
        return Ok(LeaseHeld::Held { deadline: None });
    }
    not_held(pool, id).await
}

/// Ask a task to stop, in ONE conditional statement so it cannot race a concurrent lease: `queued`
/// → `cancelled`, `leased` → `cancelling` (not leasable), terminal → untouched.
pub(crate) async fn cancel(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
) -> Result<Option<RelayCancel>> {
    let new_status: Option<String> = sqlx::query_scalar(
        "UPDATE relay_tasks \
         SET status = CASE WHEN status='queued' THEN 'cancelled' ELSE 'cancelling' END, \
             updated_at = $2 \
         WHERE id = $1 AND status IN ('queued','leased') \
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
        Some("cancelled") => return Ok(Some(RelayCancel::Cancelled)),
        Some(_) => return Ok(Some(RelayCancel::Cancelling)),
        None => {}
    }
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT status FROM relay_tasks WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)",
    )
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    Ok(existing.map(|status| RelayCancel::AlreadyFinished { status }))
}

/// Settle a leased task, fenced. `None` is the operator-shaped settle, which waives the ownership
/// condition but never the liveness one.
pub(crate) async fn settle(
    pool: &PgPool,
    id: &str,
    fence: Option<DateTime<Utc>>,
    outcome: &RelayOutcome,
) -> Result<RelaySettle> {
    let mut tx = pool.begin().await.map_err(pgerr)?;
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM relay_tasks WHERE id = $1 FOR UPDATE"
    ))
    .bind(id.to_string())
    .fetch_optional(&mut *tx)
    .await
    .map_err(pgerr)?;
    let Some(task) = row.as_ref().map(from_row).transpose()? else {
        return Ok(RelaySettle::NoSuchTask); // tx rolls back on drop
    };
    let holds = is_live_lease(&task.status) && fence.is_none_or(|f| task.lease_fence == Some(f));
    if !holds {
        return Ok(RelaySettle::NotHeld {
            status: task.status,
            fence: task.lease_fence,
        });
    }
    let now = Utc::now();
    let now_s = fmt_ts(now);
    // A cancelled run terminates as `cancelled` whatever it reports: an operator stopped it, so its
    // outcome is not a verdict on the action and must not consume the retry budget.
    let cancelling = task.status == RelayStatus::Cancelling.as_str();
    match outcome {
        RelayOutcome::Succeeded(result) => {
            let status = if cancelling { "cancelled" } else { "succeeded" };
            sqlx::query(
                "UPDATE relay_tasks SET status=$4, result=$2, error=NULL, \
                     lease_deadline=NULL, lease_fence=NULL, updated_at=$3 WHERE id=$1",
            )
            .bind(id.to_string())
            .bind(json_or_null(result)?)
            .bind(now_s)
            .bind(status.to_string())
            .execute(&mut *tx)
            .await
            .map_err(pgerr)?;
        }
        RelayOutcome::Failed(err) => {
            // `failures` — not `attempts` — is the budget: a device killed mid-run reported nothing
            // and must not have spent one of the task's chances. Nor does a CANCELLED run: an
            // operator stopped it, so whatever it reports is not a verdict on the action.
            let failures = task.failures + u32::from(!cancelling);
            let (status, next) = if cancelling {
                ("cancelled", task.next_attempt_at)
            } else if failures >= task.max_attempts {
                ("dead", task.next_attempt_at)
            } else {
                (
                    "queued",
                    now + Duration::seconds(task.retry_interval_secs as i64),
                )
            };
            sqlx::query(
                "UPDATE relay_tasks SET status=$2, error=$3, next_attempt_at=$4, failures=$6, \
                     lease_deadline=NULL, lease_fence=NULL, updated_at=$5 WHERE id=$1",
            )
            .bind(id.to_string())
            .bind(status.to_string())
            .bind(err.clone())
            .bind(fmt_ts(next))
            .bind(now_s)
            .bind(failures as i64)
            .execute(&mut *tx)
            .await
            .map_err(pgerr)?;
        }
        RelayOutcome::Deferred {
            retry_after_secs,
            reason,
        } => {
            // Not the task's fault (e.g. the subscription window is exhausted): the lease was never
            // really used, so the claim it consumed is handed back and no failure is recorded.
            let attempts = task.attempts.saturating_sub(1);
            let delay = retry_after_secs.unwrap_or(task.retry_interval_secs);
            let status = if cancelling { "cancelled" } else { "queued" };
            sqlx::query(
                "UPDATE relay_tasks SET status=$6, attempts=$2, error=$3, \
                     next_attempt_at=$4, lease_deadline=NULL, lease_fence=NULL, updated_at=$5 \
                 WHERE id=$1",
            )
            .bind(id.to_string())
            .bind(attempts as i64)
            .bind(reason.clone().or(task.error.clone()))
            .bind(fmt_ts(now + Duration::seconds(delay as i64)))
            .bind(now_s)
            .bind(status.to_string())
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
    Ok(match updated {
        Some(t) => RelaySettle::Settled(Box::new(t)),
        None => RelaySettle::NoSuchTask,
    })
}

/// Say what the record actually holds now, so a refused holder can name what beat it.
async fn not_held(pool: &PgPool, id: &str) -> Result<LeaseHeld> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM relay_tasks WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    Ok(match row.as_ref().map(from_row).transpose()? {
        Some(t) => LeaseHeld::NotHeld {
            status: t.status,
            fence: t.lease_fence.map(Into::into),
        },
        None => LeaseHeld::NoSuchRecord,
    })
}
