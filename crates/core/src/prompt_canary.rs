//! The served-version quality loop's vocabulary: what a canary is, and how a label got where it is.
//!
//! Promotion used to be the last thing the registry observed about a version. `labels.insert(label,
//! version)` returned 200 and nothing afterwards measured whether the version now serving
//! production was any good — a regression was visible only to whoever happened to scroll
//! `/v1/scores`. [`CanaryPolicy`] is the standing question ("is the canary label worse than the
//! production label, over this window, on this much evidence?") and [`LabelChange`] is the record
//! of every answer that moved a pointer.
//!
//! Kept apart from [`crate::prompt`] so the registry's data types and the canary's policy vocabulary
//! stay one-concern-per-file.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How many label moves one prompt keeps.
///
/// The prompt row is read on every runtime fetch, so the ledger is bounded rather than unbounded:
/// the newest entries are kept and older ones drop off the front. A canary that flaps must not be
/// able to grow a hot row without limit.
pub const MAX_LABEL_HISTORY: usize = 50;

/// The default label a canary serves under.
pub const DEFAULT_CANARY_LABEL: &str = "canary";
/// The default label it is measured against.
pub const DEFAULT_PRODUCTION_LABEL: &str = "production";

/// Reserved [`LabelChange::reason`] for a move an operator (or the promote route) made.
pub const REASON_PROMOTE: &str = "promote";
/// Reserved [`LabelChange::reason`] for a move the canary sweep made on its own.
pub const REASON_CANARY_REGRESSED: &str = "canary_regressed";

/// One label move: which label, to which version, when, and why.
///
/// `reason` is free text with the two reserved spellings above, so a reader can separate "someone
/// decided this" from "the canary decided this" without parsing prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LabelChange {
    pub label: String,
    pub version: u32,
    #[serde(default = "Utc::now")]
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// When a served canary version counts as having regressed against production, and what to do
/// about it.
///
/// Every default makes the policy *safe to enable*: `auto_revert` is off, so the first thing a
/// canary does is tell you rather than act; `min_n` is an evidence floor, because a version judged
/// three times has said nothing about whether it is worse; and `max_drop` is a **relative** band, so
/// it means the same thing whatever scale the rubric scores on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CanaryPolicy {
    /// The label carrying the version under test.
    #[serde(default = "default_canary_label")]
    pub label: String,
    /// The label it is measured against.
    #[serde(default = "default_production_label")]
    pub production_label: String,
    /// Verdicts required on **each** side before the comparison may decide anything. Below it the
    /// sweep stays silent — an evidence floor, not a threshold.
    #[serde(default = "default_min_n")]
    pub min_n: u32,
    /// Trailing window the comparison reads, in seconds.
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// How far the canary's mean may fall below production's before it counts as a regression, as a
    /// fraction of production's mean (`0.05` = 5% worse).
    #[serde(default = "default_max_drop")]
    pub max_drop: f64,
    /// Move the canary's label back to the version it replaced when a regression is confirmed.
    /// **Off by default**: reverting changes what a deployment serves, and that is a decision.
    #[serde(default)]
    pub auto_revert: bool,
}

fn default_canary_label() -> String {
    DEFAULT_CANARY_LABEL.to_string()
}

fn default_production_label() -> String {
    DEFAULT_PRODUCTION_LABEL.to_string()
}

fn default_min_n() -> u32 {
    20
}

fn default_window_secs() -> u64 {
    24 * 3600
}

fn default_max_drop() -> f64 {
    0.05
}

impl Default for CanaryPolicy {
    fn default() -> Self {
        CanaryPolicy {
            label: default_canary_label(),
            production_label: default_production_label(),
            min_n: default_min_n(),
            window_secs: default_window_secs(),
            max_drop: default_max_drop(),
            auto_revert: false,
        }
    }
}

impl CanaryPolicy {
    /// Why this policy cannot be used as written, or `None` when it is well-formed. Checked at the
    /// API boundary so a nonsense policy is a 400 rather than a sweep that silently never fires.
    pub fn invalid(&self) -> Option<String> {
        if self.label.trim().is_empty() || self.production_label.trim().is_empty() {
            return Some("a canary policy needs both a canary label and a production label".into());
        }
        if self.label == self.production_label {
            return Some(
                "the canary label and the production label must differ — a label compared with \
                 itself can never regress"
                    .into(),
            );
        }
        if self.min_n == 0 {
            return Some(
                "min_n must be at least 1: a comparison over no verdicts is not a comparison"
                    .into(),
            );
        }
        if self.window_secs == 0 {
            return Some("window_secs must be non-zero".into());
        }
        if !self.max_drop.is_finite() || !(0.0..=1.0).contains(&self.max_drop) {
            return Some(
                "max_drop is a fraction of production's mean, so it lies in 0.0..=1.0".into(),
            );
        }
        None
    }

    /// The prompt tag (`"<name>@v<version>"`) a version of `name` is attributed under — the
    /// `metadata.prompt` convention `ResolvedPrompt::tag` documents and every quality read groups
    /// on. Defined here so the sweep and the registry cannot spell it differently.
    pub fn tag(name: &str, version: u32) -> String {
        format!("{name}@v{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_defaults_are_the_safe_stance() {
        let c = CanaryPolicy::default();
        assert_eq!(c.label, "canary");
        assert_eq!(c.production_label, "production");
        assert!(!c.auto_revert, "a fresh policy tells you; it does not act");
        assert!(c.min_n >= 1 && c.window_secs > 0);
        assert_eq!(c.invalid(), None);
    }

    /// A policy posted as `{}` must read as the safe defaults rather than failing to deserialize —
    /// the shape an operator actually types when turning the feature on.
    #[test]
    fn an_empty_policy_deserializes_to_the_defaults() {
        let c: CanaryPolicy = serde_json::from_value(json!({})).expect("empty policy");
        assert_eq!(c, CanaryPolicy::default());
    }

    #[test]
    fn a_policy_that_can_never_fire_is_refused_before_it_is_stored() {
        let same = CanaryPolicy {
            label: "production".into(),
            ..Default::default()
        };
        assert!(same.invalid().is_some(), "a label vs itself");

        let no_evidence = CanaryPolicy {
            min_n: 0,
            ..Default::default()
        };
        assert!(no_evidence.invalid().is_some());

        for bad in [-0.1, 1.5, f64::NAN] {
            let p = CanaryPolicy {
                max_drop: bad,
                ..Default::default()
            };
            assert!(p.invalid().is_some(), "max_drop {bad}");
        }
        assert!(CanaryPolicy {
            window_secs: 0,
            ..Default::default()
        }
        .invalid()
        .is_some());
    }

    #[test]
    fn the_tag_is_the_documented_attribution_convention() {
        assert_eq!(CanaryPolicy::tag("support-reply", 4), "support-reply@v4");
    }
}
