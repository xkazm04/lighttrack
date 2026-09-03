//! The sweep pass that finds relay work with nobody left to run it (M18).
//!
//! Admission refuses an unroutable task at the enqueue door, which is where a typo dies. It cannot
//! see the second shape of the same failure, because that one happens *after* the task is accepted:
//! the only device advertising the action was revoked, narrowed its capabilities on an upgrade, or
//! was never re-enrolled after a rebuild. The task is fine. There is simply nobody to lease it, and
//! nothing about a queued task says so — it looks exactly like a healthy backlog waiting for a
//! device that is about to poll.
//!
//! So the fleet is re-asked on a timer, in the sweep that already reaps dead leases. A queued task
//! that has gone longer than the grace window with zero eligible devices raises
//! `relay_task_unroutable` through the existing [`Alerter`](crate::alerts::Alerter).

use std::collections::BTreeMap;

use chrono::Utc;

use crate::alerts_relay::UnroutableActions;
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

const ENV_SECS: &str = "LIGHTTRACK_RELAY_UNROUTABLE_SECS";

/// How long a queued task may go unroutable before it is worth waking somebody.
///
/// Fifteen minutes, not seconds: a fleet is allowed to be briefly empty — a device restarting, a
/// laptop rebooting, an agent being upgraded — and an alert that fires during a restart is an alert
/// people learn to ignore. It is well inside the five-hour retry interval, so the alert still lands
/// long before the task's first retry, let alone its dead-letter.
const DEFAULT_SECS: i64 = 900;

/// How many queued tasks to examine per pass. A bound, not a filter: the queue is scanned newest
/// first, and an unroutable *action type* shows up on every one of its tasks, so a cap cannot hide
/// a class of failure — only the tail of an enormous backlog of it.
const SCAN_LIMIT: usize = 1_000;

/// `None` when explicitly disabled (`…_SECS=0`).
fn grace_secs() -> Option<i64> {
    let secs = std::env::var(ENV_SECS)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_SECS);
    (secs > 0).then_some(secs)
}

/// One pass: find queued tasks past the grace window whose action type no enrolled device can run,
/// and alert per action type. Never propagates — a broken alert must not stop the sweep, and a
/// backend that does not host the fleet simply has nothing to say here.
pub(crate) async fn sweep_once(st: &AppState) {
    let Some(grace) = grace_secs() else { return };
    let store = st.store.clone();
    let queued = match spawn_db(move || {
        store.list_relay_tasks(TenantScope::Operator, Some("queued"), SCAN_LIMIT)
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "relay unroutable sweep: queued tasks unavailable");
            return;
        }
    };
    let now = Utc::now();
    // Group first, ask the fleet once per action type. A backlog is usually one action type
    // repeated, and a per-task eligibility read would turn a stuck queue into a table scan storm.
    let mut by_action: BTreeMap<String, (u32, i64)> = BTreeMap::new();
    for t in &queued {
        let waited = (now - t.created_at).num_seconds();
        if waited < grace {
            continue;
        }
        let e = by_action.entry(t.action_type.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.max(waited);
    }
    if by_action.is_empty() {
        return;
    }

    let mut stuck = Vec::new();
    for (action_type, (tasks, oldest_secs)) in by_action {
        let store = st.store.clone();
        let at = action_type.clone();
        let counts = match spawn_db(move || store.count_eligible_devices(&at)).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "relay unroutable sweep: device fleet unavailable");
                return;
            }
        };
        // `enrolled == 0` is NOT silence here, unlike at the enqueue door. Admission has to admit an
        // empty fleet (it is the legacy shared-key deployment, which routes fine), but a task that
        // has been queued for a quarter of an hour with no fleet at all is exactly the "the device
        // is gone" incident this sweep exists to report.
        if counts.eligible > 0 {
            continue;
        }
        stuck.push(UnroutableActions {
            action_type,
            tasks,
            oldest_secs,
            enrolled_devices: counts.enrolled,
        });
    }
    if stuck.is_empty() {
        return;
    }
    tracing::warn!(
        actions = stuck.len(),
        "relay tasks are queued with no eligible device"
    );
    st.alerts.notify_relay_unroutable(&stuck);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grace_window_defaults_on_and_can_be_switched_off() {
        std::env::remove_var(ENV_SECS);
        assert_eq!(grace_secs(), Some(DEFAULT_SECS));
        // On by default and well inside the five-hour retry interval, so the alert lands long
        // before the task's first retry, let alone its dead-letter.
        assert!(DEFAULT_SECS < lighttrack_core::RELAY_DEFAULT_RETRY_INTERVAL_SECS as i64);
        std::env::set_var(ENV_SECS, "0");
        assert_eq!(grace_secs(), None, "explicit zero is the off switch");
        std::env::set_var(ENV_SECS, "60");
        assert_eq!(grace_secs(), Some(60));
        // Junk falls back to the default rather than silently disabling the alert.
        std::env::set_var(ENV_SECS, "soon");
        assert_eq!(grace_secs(), Some(DEFAULT_SECS));
        std::env::remove_var(ENV_SECS);
    }
}
