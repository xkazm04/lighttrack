//! Per-project usage limits: the rule vocabulary (metric / window / threshold / action), the
//! optional dimension scope a rule binds to, and the status a rule evaluates to against rolling
//! usage. Split by concern; every type is re-exported here so callers keep one path.

mod rule;
mod scope;
mod status;
mod threshold;

#[cfg(test)]
mod tests;

pub use rule::{LimitAction, LimitMetric, LimitRule, LimitWindow, DEFAULT_THROTTLE_START};
pub use scope::{scope_matches, LimitScope, ScopeDims};
pub use status::{CostEvidence, LimitStatus};
pub use threshold::{Escalation, Threshold, ThresholdBasis, ThresholdDimension, ThresholdKind};
