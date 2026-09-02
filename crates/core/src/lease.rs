//! Fencing tokens for anything held under a renewable lease.
//!
//! Two queues in this system hand work to a remote executor that may die, stall, or be partitioned
//! away: the job queue (`crate::job`) and the cloud→device relay (`crate::relay`). Both need the
//! same three things, and getting any of them wrong produces the same class of bug — a stale holder
//! whose late write lands on work somebody else already owns:
//!
//! 1. a **fence**: an identity stamped on the lease, carried by every write the holder makes, and
//!    compared exactly. Not a deadline — a deadline says *when*, a fence says *whose*;
//! 2. a **deadline**, moved forward by renewal on a timer, so the staleness window is detection
//!    latency and not "the longest a legitimate run may take";
//! 3. an **answer the holder can act on** when a conditioned write is refused, rather than a bare
//!    error it cannot tell apart from a network blip.
//!
//! [`LeaseFence`] is (1) and [`LeaseHeld`] is (3). The relay adopted both in M7; `Job` re-expresses
//! its existing `claimed_at` fence through [`LeaseFence`] ([`crate::Job::fence`]) so the two queues
//! describe one mechanism in one vocabulary — no behaviour change on the job side.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The identity of one lease: the instant it was granted, compared for **exact equality**.
///
/// A newtype rather than a bare `DateTime` on purpose. The two timestamps on a leased row — "when
/// did this lease start" and "when does it expire" — are the same Rust type and are trivially
/// swappable at a call site, and swapping them turns a fence check into a liveness check that
/// accepts any holder. Naming the fence makes that mistake a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct LeaseFence(DateTime<Utc>);

impl LeaseFence {
    pub fn new(at: DateTime<Utc>) -> Self {
        LeaseFence(at)
    }

    /// The instant this lease was granted — the value persisted on the row and compared on write.
    pub fn at(self) -> DateTime<Utc> {
        self.0
    }
}

impl From<DateTime<Utc>> for LeaseFence {
    fn from(at: DateTime<Utc>) -> Self {
        LeaseFence(at)
    }
}

/// What a conditioned write against a lease actually did.
///
/// `NotHeld` is deliberately a *value*, not an error: losing a lease is a normal outcome in a
/// distributed queue and the holder has to act on it (stop working, do not deliver, do not retry),
/// which it cannot do if the answer is indistinguishable from a transient failure. The payload says
/// what the record holds **now**, so the loser can name what beat it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LeaseHeld {
    /// The write landed. `deadline` is the lease's new expiry when the call extended it.
    Held {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline: Option<DateTime<Utc>>,
    },
    /// Refused: the caller does not hold this record any more.
    NotHeld {
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fence: Option<LeaseFence>,
    },
    /// There is no such record (→ 404). Distinct from `NotHeld`, which is a live one someone else owns.
    NoSuchRecord,
}

impl LeaseHeld {
    pub fn is_held(&self) -> bool {
        matches!(self, LeaseHeld::Held { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_is_an_identity_not_a_moment() {
        let now = Utc::now();
        let f = LeaseFence::new(now);
        assert_eq!(f.at(), now);
        // Equality is exact: a lease granted a nanosecond later is a DIFFERENT lease, which is the
        // whole point — "close enough" would let a reclaimed holder's late write through.
        assert_ne!(f, LeaseFence::new(now + chrono::Duration::nanoseconds(1)));
        // Transparent on the wire, so a fence round-trips as the timestamp it is.
        let j = serde_json::to_value(f).expect("serialize fence");
        assert_eq!(j, serde_json::to_value(now).expect("serialize ts"));
    }

    #[test]
    fn not_held_carries_what_beat_it() {
        let v = LeaseHeld::NotHeld {
            status: "queued".into(),
            fence: Some(LeaseFence::new(Utc::now())),
        };
        assert!(!v.is_held());
        assert!(LeaseHeld::Held { deadline: None }.is_held());
        // Tagged, so a client branches on `outcome` instead of guessing from shape.
        let j = serde_json::to_value(&v).expect("serialize verdict");
        assert_eq!(j["outcome"], "not_held");
        assert_eq!(j["status"], "queued");
    }
}
