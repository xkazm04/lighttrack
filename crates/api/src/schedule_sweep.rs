//! The scheduled-work sweep: enqueue due schedules, and reap dead relay leases.
//!
//! **Where it runs.** In the API process, for the same reason `forecast_sweep` does: the runner is
//! an optional companion (a Cloud Run deployment ships the API alone), so recurrence hosted there
//! silently doesn't happen in the deployment that most needs it. It also puts the relay's
//! dead-letter reap on a timer — that used to run only inside `lease_relay_tasks`, which means with
//! no device polling nothing ever dead-lettered and nobody was ever alerted, precisely in the
//! scenario ("the device is gone") the alert exists for.
//!
//! **On by default**, unlike the forecast sweep. That one turns a self-hosted instance into an
//! outbound notifier, which is a decision; this one is upkeep of schedules the operator wrote down
//! themselves — a stored schedule that does not fire is just a broken feature. Set
//! `LIGHTTRACK_SCHEDULE_SWEEP_SECS=0` to disable it.
//!
//! **It cannot touch the ingest hot path.** Detached task, every store call on the blocking pool,
//! and a failure on one schedule is logged and skipped rather than ending the loop.

use std::time::Duration;

use chrono::Utc;

use lighttrack_core::{JobKind, Schedule};

use crate::jobs_enqueue::enqueue;
use crate::schedules::SCHEDULE_ID_KEY;
use crate::state::{spawn_db, AppState};

const ENV_SECS: &str = "LIGHTTRACK_SCHEDULE_SWEEP_SECS";

/// Default cadence. A minute is fine: the sweep is one indexed range read plus a write per firing,
/// and `Schedule::MIN_INTERVAL_SECS` means nothing can be due more often than this anyway.
const DEFAULT_SECS: u64 = 60;
/// Floor, so a misconfiguration cannot turn the sweep into a spin loop.
const MIN_SECS: u64 = 10;

#[derive(Clone, Copy)]
pub(crate) struct SweepConfig {
    pub(crate) interval: Duration,
}

impl SweepConfig {
    /// `None` only when explicitly disabled (`…_SECS=0`).
    pub(crate) fn from_env() -> Option<Self> {
        let secs = std::env::var(ENV_SECS)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SECS);
        (secs > 0).then(|| SweepConfig {
            interval: Duration::from_secs(secs.max(MIN_SECS)),
        })
    }
}

pub(crate) fn describe(cfg: Option<SweepConfig>) -> String {
    match cfg {
        None => format!("off ({ENV_SECS}=0)"),
        Some(c) => format!("every {}s", c.interval.as_secs()),
    }
}

/// Start the sweep loop as a detached task. No-op when disabled.
pub(crate) fn spawn(st: AppState, cfg: Option<SweepConfig>) {
    let Some(cfg) = cfg else { return };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick fires immediately; spend it so startup stays quiet
        loop {
            ticker.tick().await;
            sweep_once(&st).await;
        }
    });
}

/// One pass: reap dead relay leases, then fire every due schedule. Never panics and never
/// propagates — a broken schedule must not stop the others, or the loop.
pub(crate) async fn sweep_once(st: &AppState) {
    reap_relay(st).await;
    let store = st.store.clone();
    let due = match spawn_db(move || store.due_schedules(Utc::now())).await {
        Ok(v) => v,
        // `Unsupported` is the expected answer on a backend that does not host the table; it is a
        // declared capability gap, not an incident, so it must not log at error level every minute.
        Err(e) => {
            tracing::debug!(error = %e, "schedule sweep: due_schedules unavailable");
            return;
        }
    };
    let mut fired = 0usize;
    for s in due {
        match fire(st, &s).await {
            Ok(true) => fired += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(
                schedule_id = %s.id, kind = %s.kind, error = %e,
                "schedule sweep: could not fire a due schedule (will retry next tick)"
            ),
        }
        tokio::task::yield_now().await;
    }
    if fired > 0 {
        tracing::info!(fired, "schedule sweep enqueued due work");
    }
}

/// Fire one schedule: enqueue its job unless one is already in flight, then push `next_due` out.
///
/// **Idempotency first.** A schedule whose previous job is still queued or running is skipped, so a
/// benchmark that takes longer than its own interval never stacks a second copy of itself on top of
/// the first — the rule the old benchmark-recurrence sweep had, kept.
///
/// `next_due` moves whether or not a job was enqueued. A skipped schedule that kept an overdue
/// `next_due` would be re-examined on every single tick, which is a busy loop wearing the shape of
/// a schedule.
async fn fire(st: &AppState, s: &Schedule) -> Result<bool, crate::error::ApiError> {
    let mut updated = s.clone();
    updated.next_due = s.advance_from(Utc::now());

    let Some(kind) = s.kind() else {
        tracing::warn!(
            schedule_id = %s.id, kind = %s.kind,
            "schedule names a job kind this build does not know; skipping (its next_due still moves \
             so it does not re-examine every tick)"
        );
        save(st, &updated).await?;
        return Ok(false);
    };
    if in_flight(st, s, kind).await? {
        tracing::debug!(schedule_id = %s.id, "previous job still in flight; not stacking another");
        save(st, &updated).await?;
        return Ok(false);
    }

    // Stamp the schedule id into the payload so the job can name what produced it — the link
    // `GET /v1/schedules/:id/runs` follows, and the in-flight check above reads.
    let mut payload = s.payload.clone();
    if !payload.is_object() {
        payload = serde_json::json!({});
    }
    if let Some(o) = payload.as_object_mut() {
        o.insert(SCHEDULE_ID_KEY.into(), serde_json::json!(s.id));
    }
    let job = enqueue(st, kind, payload).await?;
    updated.last_job_id = Some(job.id);
    save(st, &updated).await?;
    Ok(true)
}

/// Whether this schedule already has a queued or running job.
async fn in_flight(
    st: &AppState,
    s: &Schedule,
    kind: JobKind,
) -> Result<bool, crate::error::ApiError> {
    let store = st.store.clone();
    let id = s.id.clone();
    let found = spawn_db(move || {
        for status in ["queued", "running"] {
            for j in store.list_jobs(Some(status), 1000)? {
                let same_schedule = j
                    .payload
                    .get(SCHEDULE_ID_KEY)
                    .and_then(serde_json::Value::as_str)
                    == Some(id.as_str());
                if same_schedule && j.job_type == kind.as_str() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })
    .await?;
    Ok(found)
}

async fn save(st: &AppState, s: &Schedule) -> Result<(), crate::error::ApiError> {
    let store = st.store.clone();
    let s2 = s.clone();
    spawn_db(move || store.update_schedule(&s2)).await?;
    Ok(())
}

/// Dead-letter relay tasks whose device never came back, and alert on them.
///
/// This is the half that had no home. `sweep_relay_dead` ran only inside `lease_relay_tasks`, so a
/// fleet with no device polling — which is exactly what "the device died" looks like from the cloud
/// — never reaped anything and never raised the alert that says so.
async fn reap_relay(st: &AppState) {
    let store = st.store.clone();
    match spawn_db(move || store.sweep_relay_dead()).await {
        Ok(dead) if !dead.is_empty() => {
            tracing::warn!(
                count = dead.len(),
                "relay tasks dead-lettered by the timed sweep"
            );
            st.alerts.notify_relay_dead(&dead);
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "schedule sweep: relay reap unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_is_on_by_default_and_floors_its_interval() {
        // Unlike the forecast sweep, absence means ON: a stored schedule that does not fire is a
        // broken feature, not a respected default.
        std::env::remove_var(ENV_SECS);
        assert_eq!(
            SweepConfig::from_env().map(|c| c.interval),
            Some(Duration::from_secs(DEFAULT_SECS))
        );
        // Explicit zero is the off switch.
        std::env::set_var(ENV_SECS, "0");
        assert!(SweepConfig::from_env().is_none());
        // A misconfiguration cannot make it a spin loop.
        std::env::set_var(ENV_SECS, "1");
        assert_eq!(
            SweepConfig::from_env().map(|c| c.interval),
            Some(Duration::from_secs(MIN_SECS))
        );
        // Junk falls back to the default rather than silently disabling the sweep.
        std::env::set_var(ENV_SECS, "later");
        assert!(SweepConfig::from_env().is_some());
        std::env::remove_var(ENV_SECS);
    }
}
