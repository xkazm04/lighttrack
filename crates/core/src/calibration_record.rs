//! A calibration **result** as a stored row, and the trust question gates ask of it (M11).
//!
//! [`super::calibration::Agreement`] is the pure computation; it reached stdout and an exit code
//! and then evaporated. The κ *history* was recovered by scanning the newest 500 scores for a
//! reserved rubric name and parsing a metrics blob out of `Score.reasoning` — which meant the one
//! fact that says "this judge can be believed" was a string in a free-text column, invisible to
//! every gate that should have been asking about it.
//!
//! A [`CalibrationRecord`] is that fact as a row keyed by `(rubric, judge)`, and [`JudgeTrust`] is
//! the three-valued answer a gate gets back. Three-valued on purpose: **unknown** is not
//! **untrusted**. A judge nobody has ever calibrated has not failed a check — it has taken none —
//! and a policy that wants to block on that (`Project::require_trusted_judge`) must be able to say
//! so explicitly rather than have the absence of evidence silently read as either verdict.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::calibration::Agreement;

/// One completed judge↔human calibration, stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationRecord {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    /// The judge that was measured, as a canonical `[provider/]model` string. Half of the key: the
    /// same rubric calibrated against two judges is two independent facts, and trusting one because
    /// the other passed is exactly the uncalibrated-gate failure.
    pub judge: String,
    /// The rubric the judging was done under. `None` = a freeform (rubric-less) calibration, which
    /// answers only for freeform judging — a rubric's own trust is never inherited from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_id: Option<String>,
    /// The labeled set this was measured on, when it came from one rather than from a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    /// The dataset generation measured, so a record does not silently claim to describe a set that
    /// has since been added to. Written by whoever knows the generation; `None` on a file import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_version: Option<u32>,
    pub kappa: f64,
    pub pearson: f64,
    pub mae: f64,
    pub rmse: f64,
    /// Pairs measured. A record with a small `n` is still a record — the honest thing is to store
    /// it with its `n` rather than to refuse it, so a reader can see that the trust rests on 12
    /// cases (D15's own caveat) instead of guessing.
    pub n: u32,
    /// The bar `kappa` was compared against, stored beside it: raising the bar later must not
    /// silently re-verdict history.
    pub kappa_bar: f64,
    /// `kappa >= kappa_bar` at the time of measurement.
    pub trusted: bool,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

impl CalibrationRecord {
    /// Build a record from a computed [`Agreement`]. The single conversion point, so the stored row
    /// and the printed table can never disagree about what κ was.
    pub fn from_agreement(
        project_id: &str,
        judge: &str,
        rubric_id: Option<&str>,
        a: &Agreement,
    ) -> Self {
        Self {
            id: crate::new_id(),
            project_id: project_id.to_string(),
            judge: judge.to_string(),
            rubric_id: rubric_id.map(str::to_string),
            dataset_id: None,
            dataset_version: None,
            kappa: a.cohen_kappa,
            pearson: a.pearson,
            mae: a.mae,
            rmse: a.rmse,
            n: a.n as u32,
            kappa_bar: a.kappa_bar,
            trusted: a.trusted,
            created_at: Utc::now(),
        }
    }

    /// The trust this record asserts.
    pub fn trust(&self) -> JudgeTrust {
        if self.trusted {
            JudgeTrust::Trusted
        } else {
            JudgeTrust::Untrusted
        }
    }
}

/// Whether a judge may be believed for a rubric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeTrust {
    /// A calibration exists and cleared its κ bar.
    Trusted,
    /// A calibration exists and did **not** clear its bar.
    Untrusted,
    /// Nobody has measured this `(rubric, judge)` pair. The default, and never conflated with
    /// `Untrusted`: one is a failed check, the other is no check.
    #[default]
    Unknown,
}

impl JudgeTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            JudgeTrust::Trusted => "trusted",
            JudgeTrust::Untrusted => "untrusted",
            JudgeTrust::Unknown => "unknown",
        }
    }

    /// Whether a `require_trusted_judge` project must block on this. Both non-trusted answers
    /// block: a gate that promotes on an unmeasured judge is the failure M11 exists to close.
    pub fn blocks_under_policy(self) -> bool {
        self != JudgeTrust::Trusted
    }
}

/// The answer `GET /v1/judges/trust` gives, and the block gates embed in their own response.
///
/// The deciding record travels with the verdict rather than being fetched separately, because
/// "untrusted" is not actionable on its own — an operator needs the κ, the `n` and the date to know
/// whether to recalibrate or to change judges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeTrustVerdict {
    pub trust: JudgeTrust,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationRecord>,
}

impl JudgeTrustVerdict {
    /// The verdict a `latest_calibration` lookup implies. `None` ⇒ [`JudgeTrust::Unknown`].
    pub fn from_record(record: Option<CalibrationRecord>) -> Self {
        Self {
            trust: record
                .as_ref()
                .map(CalibrationRecord::trust)
                .unwrap_or_default(),
            calibration: record,
        }
    }

    pub fn unknown() -> Self {
        Self {
            trust: JudgeTrust::Unknown,
            calibration: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::agreement;

    fn record(kappa_bar: f64) -> CalibrationRecord {
        let a = agreement(&[(0.9, 0.9), (0.2, 0.2), (0.8, 0.8)], 0.7, kappa_bar);
        CalibrationRecord::from_agreement("p", "anthropic/claude-haiku-4-5", Some("rb1"), &a)
    }

    #[test]
    fn a_record_carries_the_bar_it_was_judged_against() {
        let r = record(0.6);
        assert!(r.trusted);
        assert_eq!(r.kappa_bar, 0.6);
        assert_eq!(r.n, 3);
        assert_eq!(r.trust(), JudgeTrust::Trusted);
        // A bar nothing can clear flips the verdict, and the record says which bar it used — so
        // raising the bar later cannot silently re-verdict a stored measurement.
        let strict = record(1.5);
        assert!(!strict.trusted);
        assert_eq!(strict.trust(), JudgeTrust::Untrusted);
    }

    /// The distinction the whole type exists for: no measurement is not a failed measurement.
    #[test]
    fn absence_is_unknown_and_never_untrusted() {
        let v = JudgeTrustVerdict::from_record(None);
        assert_eq!(v.trust, JudgeTrust::Unknown);
        assert!(v.calibration.is_none());
        assert_ne!(v.trust, JudgeTrust::Untrusted);
        // …but a policy that demands trust blocks on both, which is the other half of the rule.
        assert!(JudgeTrust::Unknown.blocks_under_policy());
        assert!(JudgeTrust::Untrusted.blocks_under_policy());
        assert!(!JudgeTrust::Trusted.blocks_under_policy());
    }

    #[test]
    fn a_verdict_carries_the_record_that_decided_it() {
        let v = JudgeTrustVerdict::from_record(Some(record(0.6)));
        assert_eq!(v.trust, JudgeTrust::Trusted);
        assert_eq!(v.calibration.expect("record travels with verdict").n, 3);
    }
}
