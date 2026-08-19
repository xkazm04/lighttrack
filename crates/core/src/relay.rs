//! Cloud→device relay task types (see `docs/RELAY.md`).
//!
//! Apps enqueue an `action_type` + JSON params on the cloud instance; the enrolled local device
//! leases due tasks over outbound HTTPS, executes them against its local (gitignored) action
//! library with the Claude Code CLI, and settles each task with an outcome. Prompts, allowed
//! tools, and connector credentials never transit the cloud — the payload carries parameters only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Failed attempts allowed before a task dead-letters.
pub const RELAY_DEFAULT_MAX_ATTEMPTS: u32 = 4;
/// Delay between attempts: 5 hours — one Claude subscription usage window.
pub const RELAY_DEFAULT_RETRY_INTERVAL_SECS: u32 = 18_000;

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
    /// Terminal failure — `max_attempts` exhausted.
    Dead,
}

impl RelayStatus {
    /// Every status, so consumers can enumerate/validate the closed vocabulary.
    pub const ALL: [RelayStatus; 4] = [
        RelayStatus::Queued,
        RelayStatus::Leased,
        RelayStatus::Succeeded,
        RelayStatus::Dead,
    ];

    /// The persisted/wire literal for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            RelayStatus::Queued => "queued",
            RelayStatus::Leased => "leased",
            RelayStatus::Succeeded => "succeeded",
            RelayStatus::Dead => "dead",
        }
    }

    /// Parse a wire literal back to a status, or `None` when it is not part of the vocabulary.
    pub fn from_wire(s: &str) -> Option<RelayStatus> {
        RelayStatus::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// One queued unit of device work. Status: `queued` | `leased` | `succeeded` | `dead`.
/// A failed attempt goes back to `queued` (with `error` recorded and `next_attempt_at` pushed
/// out) until `max_attempts` is exhausted, which flips it to `dead`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Attempts consumed (incremented when leased; a `Deferred` settle hands one back).
    #[serde(default)]
    pub attempts: u32,
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
}
