//! What a conditioned relay write actually did.
//!
//! Both verdicts are *values*, not errors, for the same reason [`crate::LeaseHeld`] is: losing a
//! task, or asking to cancel one that already finished, are normal outcomes a caller has to act on,
//! and an error is indistinguishable from a transient failure. Each carries what the record holds
//! **now**, so the loser can name what beat it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::relay::RelayTask;

/// What a settle attempt did, so a device learns whether its report landed instead of assuming it.
///
/// The unconditioned settle this replaces checked only `status == "leased"`, which is a liveness
/// question where an ownership question was needed: a device whose lease had expired and whose task
/// had been re-leased elsewhere found the row `leased` — by someone else — and wrote its result
/// over the run in progress. A settle is a transition like any other and goes through the same
/// conditioned door: *record this outcome where the task is still leased and still mine*.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RelaySettle {
    /// The report landed; the updated row. Boxed: a task row dwarfs the two refusal variants, and
    /// an enum sized to its largest variant would make every settle return move a full row.
    Settled(Box<RelayTask>),
    /// Refused. The caller does not hold this task any more — its lease expired and someone
    /// reclaimed it, an operator cancelled it, or it already reached a terminal state. `status` and
    /// `fence` are what the record says NOW, so the loser can log what beat it.
    ///
    /// A *duplicate* report from the same still-holding device is not this: it settles the task
    /// once and answers `Settled` with the already-terminal row, so a retried report is harmless.
    NotHeld {
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fence: Option<DateTime<Utc>>,
    },
    /// There is no such task (→ 404).
    NoSuchTask,
}

/// What a cancel request did to a relay task. Mirrors [`crate::JobCancel`] — the same three honest
/// answers, so the API never reports "cancelled" for a run that is still spending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RelayCancel {
    /// It was queued — nothing ever ran, and nothing ever will.
    Cancelled,
    /// It was leased: marked `cancelling`. The device learns at its next renewal, stops, and does
    /// not deliver.
    Cancelling,
    /// It had already reached a terminal state; nothing changed.
    AlreadyFinished { status: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settle_verdict_says_which_of_the_three_things_happened() {
        let v = RelaySettle::NotHeld {
            status: "leased".into(),
            fence: Some(Utc::now()),
        };
        let j = serde_json::to_value(&v).expect("serialize settle verdict");
        assert_eq!(j["outcome"], "not_held");
        assert_eq!(j["status"], "leased");
        let j = serde_json::to_value(RelaySettle::NoSuchTask).expect("serialize");
        assert_eq!(j["outcome"], "no_such_task");
        let j = serde_json::to_value(RelayCancel::AlreadyFinished {
            status: "succeeded".into(),
        })
        .expect("serialize cancel verdict");
        assert_eq!(j["outcome"], "already_finished");
    }
}
