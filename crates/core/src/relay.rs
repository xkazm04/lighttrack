//! Cloud→device relay task types (see `docs/RELAY.md`).
//!
//! Apps enqueue an `action_type` + JSON params on the cloud instance; the enrolled local device
//! leases due tasks over outbound HTTPS, executes them against its local (gitignored) action
//! library with the Claude Code CLI, and settles each task with an outcome. Prompts, allowed
//! tools, and connector credentials never transit the cloud — the payload carries parameters only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Failed attempts allowed before a task dead-letters. Counted in `failures` — runs that actually
/// ran and failed — never in `attempts`, which a device death also burns.
pub const RELAY_DEFAULT_MAX_ATTEMPTS: u32 = 4;
/// Delay between attempts: 5 hours — one Claude subscription usage window.
pub const RELAY_DEFAULT_RETRY_INTERVAL_SECS: u32 = 18_000;

/// How many times a task may be reclaimed from a device that vanished mid-run before it
/// dead-letters, independently of `max_attempts`.
///
/// The two counters answer different questions and must not be one number (`job.rs` learned this
/// first): `failures` is "the work keeps failing", `stale_reclaims` is "devices keep dying on this
/// task". A task that reliably kills its device would otherwise loop forever on a retry budget it
/// never consumes, because a device that dies never reports a failure.
pub const RELAY_MAX_STALE_RECLAIMS: u32 = 3;

/// Prefix on a stored relay error meaning **the device died**, not that the action failed.
pub const RELAY_ERROR_DEVICE_LOST: &str =
    "device lost: the device holding this task did not renew its lease before the deadline \
     (crashed, killed, or offline) — this is not an action failure";

/// The one authority for a [`RelayTask`]'s lifecycle vocabulary.
///
/// The persisted `RelayTask::status` stays a `String` for schema/back-compat (an out-of-vocabulary
/// row read from an older store deserializes rather than hard-failing), but this enum is what mints
/// and validates those literals so the vocabulary can be *enumerated* — a list filter can reject an
/// unknown value instead of silently returning an empty page, and a new state added here forces
/// every `match` that folds over `ALL` to consider it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStatus {
    /// Enqueued and awaiting (or between) lease attempts.
    Queued,
    /// Held by a device under an unexpired lease.
    Leased,
    /// Terminal success.
    Succeeded,
    /// Terminal failure — the retry budget or the device-death budget is exhausted.
    Dead,
    /// A *leased* task an operator asked to stop: the device notices at its next renewal and stops
    /// without delivering. Deliberately outside the leasable set, so the reclaim path can never
    /// hand a cancelled task to a second device.
    Cancelling,
    /// Terminal: cancelled before or during execution, and never retried.
    Cancelled,
}

impl RelayStatus {
    /// Every status, so consumers can enumerate/validate the closed vocabulary.
    pub const ALL: [RelayStatus; 6] = [
        RelayStatus::Queued,
        RelayStatus::Leased,
        RelayStatus::Succeeded,
        RelayStatus::Dead,
        RelayStatus::Cancelling,
        RelayStatus::Cancelled,
    ];

    /// The persisted/wire literal for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            RelayStatus::Queued => "queued",
            RelayStatus::Leased => "leased",
            RelayStatus::Succeeded => "succeeded",
            RelayStatus::Dead => "dead",
            RelayStatus::Cancelling => "cancelling",
            RelayStatus::Cancelled => "cancelled",
        }
    }

    /// Whether a task in this status is finished for good — never leased, retried, or settled again.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RelayStatus::Succeeded | RelayStatus::Dead | RelayStatus::Cancelled
        )
    }

    /// Parse a wire literal back to a status, or `None` when it is not part of the vocabulary.
    pub fn from_wire(s: &str) -> Option<RelayStatus> {
        RelayStatus::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// One queued unit of device work. `status` is one of [`RelayStatus::ALL`] (`queued` | `leased` |
/// `succeeded` | `dead` | `cancelling` | `cancelled`). A failed attempt goes back to `queued` (with
/// `error` recorded and `next_attempt_at` pushed out) until `max_attempts` is exhausted, which
/// flips it to `dead`; a cancel moves a queued task to `cancelled` and a leased one through
/// `cancelling`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RelayTask {
    #[serde(default = "crate::new_id")]
    pub id: String,
    pub project_id: String,
    /// Free-form originator tag (which app/service enqueued it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The contract string resolved against the device's local action library,
    /// e.g. `xprice/reprice-summary`.
    pub action_type: String,
    /// JSON parameters substituted into the action's local prompt template.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
    #[serde(default = "default_status")]
    pub status: String,
    /// How many times a device has **leased** this task. Bumped inside the atomic lease, so it
    /// counts device deaths too — which is why it is no longer what decides a retry (see
    /// [`RelayTask::failures`]). A `Deferred` settle still hands one back, because a deferral is a
    /// lease that was never really used.
    #[serde(default)]
    pub attempts: u32,
    /// How many times the task actually RAN and failed (the device reported a failure). This — not
    /// `attempts` — is the retry budget measured against `max_attempts`, so a device killed
    /// mid-run no longer burns one of the task's chances.
    #[serde(default)]
    pub failures: u32,
    /// How many times this task was reclaimed from a device that held it past its lease deadline
    /// without finishing: the count of *device deaths*, kept apart from `failures` so an operator
    /// can tell "the action keeps failing" from "the laptop keeps sleeping". Dead-letters at
    /// [`RELAY_MAX_STALE_RECLAIMS`].
    #[serde(default)]
    pub stale_reclaims: u32,
    /// The holding device's own fencing token: the instant its lease was granted, compared for
    /// exact equality on every write it makes about this task.
    ///
    /// Without it, `settle` could only ask "is this task still leased?" — which a task re-leased to
    /// a *second* device answers yes to, so the first device's late report landed on the second
    /// device's run. Renewal moves `lease_deadline`, never this: one device's lease keeps one
    /// identity for its whole run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_fence: Option<DateTime<Utc>>,
    /// Free-text liveness detail the device publishes while running, visible in `get_relay_task`.
    /// Carried on `/progress`, never on the renewal — liveness must never wait on the work having
    /// something to say, or a live-but-stuck device reads as a dead one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u32,
    /// Client-supplied dedupe key, unique per project — re-enqueueing returns the existing task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Which device holds (or last held) the lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// A lease past this deadline is reclaimable (re-leased, or dead if attempts are exhausted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_deadline: Option<DateTime<Utc>>,
    /// The task is not leasable before this instant (retry backoff).
    #[serde(default = "Utc::now")]
    pub next_attempt_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

fn default_status() -> String {
    RelayStatus::Queued.as_str().to_string()
}

fn default_max_attempts() -> u32 {
    RELAY_DEFAULT_MAX_ATTEMPTS
}

fn default_retry_interval() -> u32 {
    RELAY_DEFAULT_RETRY_INTERVAL_SECS
}

/// How a device settles a leased task.
#[derive(Debug, Clone)]
pub enum RelayOutcome {
    /// Terminal success with the structured result payload.
    Succeeded(Value),
    /// A real failure — the consumed attempt stands; requeues at `now + retry_interval`
    /// or dead-letters once attempts are exhausted.
    Failed(String),
    /// Not attemptable right now (e.g. the subscription window is exhausted) — hands the
    /// attempt back and requeues after `retry_after_secs` (default: the task's retry interval).
    Deferred {
        retry_after_secs: Option<u32>,
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_status_vocabulary_round_trips_and_rejects_outsiders() {
        // Every enumerated status maps to a literal and back — the authority and its wire form agree.
        for st in RelayStatus::ALL {
            assert_eq!(RelayStatus::from_wire(st.as_str()), Some(st));
        }
        // A plausible-but-wrong term (a settle-vocabulary word that is NOT a task status) is rejected
        // rather than silently accepted — this is what lets `?status=failed` 400 instead of paging
        // empty.
        assert_eq!(RelayStatus::from_wire("failed"), None);
        assert_eq!(RelayStatus::from_wire("running"), None);
        // The default a fresh task is minted with comes from the same authority.
        assert_eq!(default_status(), RelayStatus::Queued.as_str());
    }

    #[test]
    fn cancellation_joined_the_vocabulary_without_widening_terminality() {
        // `cancelling` is a LIVE state: a device is still running, so it is not terminal — but it is
        // also not leasable, which is what stops a second device picking up a cancelled task.
        assert!(!RelayStatus::Cancelling.is_terminal());
        assert!(RelayStatus::Cancelled.is_terminal());
        assert!(RelayStatus::Succeeded.is_terminal());
        assert!(RelayStatus::Dead.is_terminal());
        assert!(!RelayStatus::Queued.is_terminal());
        assert!(!RelayStatus::Leased.is_terminal());
    }

    #[test]
    fn a_task_read_from_an_older_store_gains_the_new_counters_as_zero() {
        // Back-compat is the point: a row written before M7 carries no failures/stale_reclaims/
        // fence/progress, and must deserialize rather than fail the whole list read.
        let t: RelayTask = serde_json::from_value(serde_json::json!({
            "id": "t1",
            "project_id": "p1",
            "action_type": "a/b",
            "next_attempt_at": "2026-01-01T00:00:00.000000000Z",
        }))
        .expect("legacy relay row");
        assert_eq!(t.failures, 0);
        assert_eq!(t.stale_reclaims, 0);
        assert!(t.lease_fence.is_none());
        assert!(t.progress.is_none());
        assert_eq!(t.status, RelayStatus::Queued.as_str());
    }
}
