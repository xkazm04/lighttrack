//! Per-case service limits: what a target must hold to *besides* scoring well.
//!
//! A rubric grades the answer. It says nothing about a configuration that reaches the right answer
//! at eight seconds and four cents a case, which in production is not the same product as the one
//! that reaches it in one second for a tenth of a cent. The cost–quality frontier (§2b) ranks those
//! trade-offs across targets, but ranking is not a gate: a target can sit on the frontier and still
//! be unusable for the job.
//!
//! So a limit is a **pass/fail assertion on the case**, in the same place the rubric's verdict is.
//! A case that breaches one fails, however well it scored, and the run's pass rate carries it.
//!
//! Two deliberate refusals:
//! - **A limit that could not be checked never passes quietly.** An unpriced model has no cost to
//!   compare, and a target that reported no latency has no time; such a case is counted as
//!   *unchecked* and named, rather than being admitted because nothing contradicted the limit.
//! - **The score is untouched.** Quality is still quality; the limit decides `pass`. Folding a
//!   latency breach into the number a judge produced would make two different measurements
//!   indistinguishable in every report that reads the score.

use serde::{Deserialize, Serialize};

/// Per-case limits on one target's generation. Both are optional; a limit that is not set is not
/// checked at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CaseLimits {
    /// Most a single case's generation may cost, USD **per candidate** — so a `--gen-samples 3` run
    /// is held to the same per-call bar as a single-draw one rather than three times it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Longest a single case's generation may take, milliseconds per candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_latency_ms: Option<u64>,
}

/// What the limits made of one case.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LimitCheck {
    /// Limits this case exceeded, by name (`cost`, `latency`).
    pub breached: Vec<&'static str>,
    /// Limits that were set but had nothing to compare against — an unpriced model, a target that
    /// reported no latency. Never silently a pass.
    pub unchecked: Vec<&'static str>,
}

impl LimitCheck {
    pub fn passed(&self) -> bool {
        self.breached.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.breached.is_empty() && self.unchecked.is_empty()
    }
}

impl CaseLimits {
    /// True when no limit is set — the shape every benchmark written before this had.
    pub fn is_empty(&self) -> bool {
        self.max_cost_usd.is_none() && self.max_latency_ms.is_none()
    }

    /// `Err(reason)` when a limit cannot be satisfied by any run. A non-positive or non-finite
    /// ceiling fails every case by construction, which is never what an operator meant — and it
    /// would turn a whole target red for a typo, at the door of a benchmark that gates deploys.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(c) = self.max_cost_usd {
            if !c.is_finite() || c <= 0.0 {
                return Err(format!(
                    "limits.max_cost_usd must be a positive number (got {c}); a ceiling of zero or \
                     less fails every case by construction"
                ));
            }
        }
        if self.max_latency_ms == Some(0) {
            return Err(
                "limits.max_latency_ms must be greater than zero; a ceiling of zero fails every \
                 case by construction"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Judge one case's measured generation against these limits.
    ///
    /// `cost_usd` and `latency_ms` are **per candidate** means, and `None` means "not measured" —
    /// which is what separates a breach from an unchecked limit.
    pub fn check(&self, cost_usd: Option<f64>, latency_ms: Option<f64>) -> LimitCheck {
        let mut out = LimitCheck::default();
        if let Some(max) = self.max_cost_usd {
            match cost_usd {
                Some(c) if c > max => out.breached.push("cost"),
                Some(_) => {}
                None => out.unchecked.push("cost"),
            }
        }
        if let Some(max) = self.max_latency_ms {
            match latency_ms {
                Some(l) if l > max as f64 => out.breached.push("latency"),
                Some(_) => {}
                None => out.unchecked.push("latency"),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits(cost: Option<f64>, latency: Option<u64>) -> CaseLimits {
        CaseLimits {
            max_cost_usd: cost,
            max_latency_ms: latency,
        }
    }

    /// **The assertion.** A case that answers well but costs or takes too much fails — which is the
    /// whole point of a limit, and is invisible to a rubric.
    #[test]
    fn a_case_over_a_ceiling_breaches_it_by_name() {
        let l = limits(Some(0.01), Some(2_000));
        assert!(l.check(Some(0.004), Some(900.0)).passed());
        assert_eq!(
            l.check(Some(0.02), Some(900.0)).breached,
            vec!["cost"],
            "the dear case fails, however well it scored"
        );
        assert_eq!(
            l.check(Some(0.004), Some(9_000.0)).breached,
            vec!["latency"]
        );
        assert_eq!(
            l.check(Some(0.02), Some(9_000.0)).breached,
            vec!["cost", "latency"],
            "both are reported, never just the first"
        );
        // The boundary itself passes: a 2000ms ceiling means 2000ms is allowed.
        assert!(l.check(Some(0.01), Some(2_000.0)).passed());
    }

    /// **A limit with nothing to compare against is not a pass.** An unpriced model reports no cost;
    /// admitting the case because nothing contradicted the ceiling is how a cost gate quietly stops
    /// gating.
    #[test]
    fn an_unmeasurable_limit_is_unchecked_rather_than_passed() {
        let l = limits(Some(0.01), Some(2_000));
        let c = l.check(None, Some(10.0));
        assert!(c.passed(), "nothing breached…");
        assert_eq!(c.unchecked, vec!["cost"], "…but it was not checked either");
        assert_eq!(l.check(Some(0.001), None).unchecked, vec!["latency"]);
        // No limits set: nothing checked, nothing to report.
        assert!(CaseLimits::default().check(None, None).is_empty());
        assert!(CaseLimits::default().is_empty());
    }

    /// A ceiling no run could ever pass is refused at the door rather than turning a target red.
    #[test]
    fn an_impossible_ceiling_is_refused() {
        assert!(limits(Some(0.01), Some(1)).validate().is_ok());
        assert!(limits(Some(0.0), None).validate().is_err());
        assert!(limits(Some(-1.0), None).validate().is_err());
        assert!(limits(Some(f64::NAN), None).validate().is_err());
        assert!(limits(None, Some(0)).validate().is_err());
        assert!(CaseLimits::default().validate().is_ok());
    }

    /// Absent keys stay absent: a stored matrix that never heard of limits round-trips unchanged.
    #[test]
    fn empty_limits_serialize_to_nothing() {
        assert_eq!(
            serde_json::to_value(CaseLimits::default()).unwrap(),
            json!({})
        );
        let l: CaseLimits = serde_json::from_value(json!({ "max_latency_ms": 1500 })).unwrap();
        assert_eq!(l.max_latency_ms, Some(1500));
        assert!(l.max_cost_usd.is_none());
        assert_eq!(
            serde_json::to_value(&l).unwrap(),
            json!({ "max_latency_ms": 1500 })
        );
    }
}
