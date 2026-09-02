//! The conditioned half of the relay: every write a *holder* makes, plus the operator's cancel.
//!
//! One rule runs through all of it. A holder's write lands only where the task is still under a
//! live lease **and** `lease_fence` is exactly the value that holder was handed. The check the
//! fence replaces was `status == "leased"`, which asks about liveness where ownership was meant: a
//! task whose lease expired and was re-leased to a second device is still `leased`, so the first
//! device's late report landed on the second device's run and overwrote it — silently, with a
//! plausible-looking result.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use lighttrack_core::{LeaseHeld, RelayCancel, RelayOutcome, RelaySettle, RelayStatus};

use crate::codec::{fmt_ts, json_or_null};
use crate::Result;

use super::relay::{get, is_live_lease};

/// Extend the holder's lease: move `lease_deadline` forward, conditioned on the fence still being
/// this caller's and the task still live. Returns the new deadline on success.
///
/// The fence is deliberately **not** moved. On the job queue the claim timestamp is both fence and
/// heartbeat, so renewal moves it; here they are separate columns, and keeping the fence still
/// means one device's lease keeps one identity for its whole run — the report it sends hours later
/// carries the same token it was given at lease time.
pub(super) fn renew(
    conn: &Connection,
    id: &str,
    fence: DateTime<Utc>,
    lease_secs: i64,
) -> Result<LeaseHeld> {
    let now = Utc::now();
    let deadline = now + Duration::seconds(lease_secs.max(0));
    let n = conn.execute(
        "UPDATE relay_tasks SET lease_deadline = ?3, updated_at = ?4 \
         WHERE id = ?1 AND lease_fence = ?2 AND status IN ('leased','cancelling')",
        params![id, fmt_ts(fence), fmt_ts(deadline), fmt_ts(now)],
    )?;
    if n > 0 {
        return Ok(LeaseHeld::Held {
            deadline: Some(deadline),
        });
    }
    not_held(conn, id)
}

/// Publish liveness detail. Same condition as the renewal, on its own endpoint: progress must never
/// ride the heartbeat, or a device that is alive but stuck computing something to say reads as a
/// dead one — and those are the two states the whole mechanism exists to tell apart.
pub(super) fn update_progress(
    conn: &Connection,
    id: &str,
    fence: DateTime<Utc>,
    progress: &str,
) -> Result<LeaseHeld> {
    let n = conn.execute(
        "UPDATE relay_tasks SET progress = ?3, updated_at = ?4 \
         WHERE id = ?1 AND lease_fence = ?2 AND status IN ('leased','cancelling')",
        params![id, fmt_ts(fence), progress, fmt_ts(Utc::now())],
    )?;
    if n > 0 {
        return Ok(LeaseHeld::Held { deadline: None });
    }
    not_held(conn, id)
}

/// Ask a task to stop. One conditional statement, so it cannot race a concurrent lease into an
/// inconsistent state: `queued` → `cancelled` outright, `leased` → `cancelling` (which is outside
/// the leasable set, so the reclaim path can never hand it to a second device), terminal →
/// untouched and reported as already finished.
pub(super) fn cancel(
    conn: &Connection,
    project: Option<&str>,
    id: &str,
) -> Result<Option<RelayCancel>> {
    let now = fmt_ts(Utc::now());
    let scope = super::scope_and(3);
    let sql = format!(
        "UPDATE relay_tasks \
         SET status = CASE WHEN status='queued' THEN 'cancelled' ELSE 'cancelling' END, \
             updated_at = ?2 \
         WHERE id = ?1 AND status IN ('queued','leased'){scope} \
         RETURNING status"
    );
    let mut stmt = conn.prepare(&sql)?;
    let new_status: Option<String> = stmt
        .query_row(params![id, now, project], |r| r.get(0))
        .optional()?;
    match new_status.as_deref() {
        Some("cancelled") => return Ok(Some(RelayCancel::Cancelled)),
        Some(_) => return Ok(Some(RelayCancel::Cancelling)),
        None => {}
    }
    let existing_sql = format!(
        "SELECT status FROM relay_tasks WHERE id = ?1{}",
        super::scope_and(2)
    );
    let existing: Option<String> = conn
        .query_row(&existing_sql, params![id, project], |r| r.get(0))
        .optional()?;
    Ok(existing.map(|status| RelayCancel::AlreadyFinished { status }))
}

/// Settle a leased task with the device's outcome, **fenced**.
///
/// `fence` is what the holder believes it holds; `None` is the operator-shaped settle, which waives
/// the ownership condition but never the liveness one. A duplicate report from the still-holding
/// device is not a refusal — the task is terminal by then, so it answers `NotHeld` with the settled
/// row's status, which is the honest thing to say and is what the API turns into a harmless 409.
pub(super) fn settle(
    conn: &Connection,
    id: &str,
    fence: Option<DateTime<Utc>>,
    outcome: &RelayOutcome,
) -> Result<RelaySettle> {
    let Some(task) = get(conn, None, id)? else {
        return Ok(RelaySettle::NoSuchTask);
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
            let result_s = json_or_null(result)?;
            let status = if cancelling { "cancelled" } else { "succeeded" };
            conn.execute(
                "UPDATE relay_tasks SET status=?4, result=?2, error=NULL, \
                     lease_deadline=NULL, lease_fence=NULL, updated_at=?3 WHERE id=?1",
                params![id, result_s, now_s, status],
            )?;
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
            conn.execute(
                "UPDATE relay_tasks SET status=?2, error=?3, next_attempt_at=?4, failures=?6, \
                     lease_deadline=NULL, lease_fence=NULL, updated_at=?5 WHERE id=?1",
                params![id, status, err, fmt_ts(next), now_s, failures as i64],
            )?;
        }
        RelayOutcome::Deferred {
            retry_after_secs,
            reason,
        } => {
            // Not the task's fault (e.g. the subscription window is exhausted): the lease was never
            // really used, so the claim it consumed is handed back and no failure is recorded.
            let attempts = task.attempts.saturating_sub(1);
            let delay = retry_after_secs.unwrap_or(task.retry_interval_secs);
            let next = now + Duration::seconds(delay as i64);
            let status = if cancelling { "cancelled" } else { "queued" };
            conn.execute(
                "UPDATE relay_tasks SET status=?6, attempts=?2, error=?3, \
                     next_attempt_at=?4, lease_deadline=NULL, lease_fence=NULL, updated_at=?5 \
                 WHERE id=?1",
                params![
                    id,
                    attempts as i64,
                    reason.as_deref().or(task.error.as_deref()),
                    fmt_ts(next),
                    now_s,
                    status
                ],
            )?;
        }
    }
    Ok(match get(conn, None, id)? {
        Some(t) => RelaySettle::Settled(Box::new(t)),
        None => RelaySettle::NoSuchTask,
    })
}

/// Say what the record actually holds now, so a refused holder can name what beat it.
fn not_held(conn: &Connection, id: &str) -> Result<LeaseHeld> {
    Ok(match get(conn, None, id)? {
        Some(t) => LeaseHeld::NotHeld {
            status: t.status,
            fence: t.lease_fence.map(Into::into),
        },
        None => LeaseHeld::NoSuchRecord,
    })
}
