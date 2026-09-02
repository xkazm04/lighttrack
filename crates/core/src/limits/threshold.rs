//! What a limit is measured *against* — and where that number came from.
//!
//! A cap used to be one hand-typed constant. That is fine for "never spend more than $500/day" and
//! useless for the question the margin surfaces actually answer ("this customer pays us $412/month,
//! stop burning more than 80% of that on them"): the second number goes stale on the next invoice,
//! so nobody keeps it current and the guardrail quietly becomes decoration.
//!
//! [`Threshold`] therefore has two forms — a fixed number, and a share of recognized revenue that is
//! **resolved at evaluation time**. Resolution can fail (no revenue data for the window), and a cap
//! that cannot be measured must never turn into a surprise block, so the unknown case resolves to
//! `+inf` (nothing breaches) and says so in its [`ThresholdBasis`] rather than guessing.
//!
//! Wire compatibility is load-bearing: `Threshold` is `#[serde(untagged)]` with `Fixed` first, so
//! every stored rule whose `threshold` is a bare number deserializes byte-identically to what it did
//! before this module existed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::LimitAction;

/// The billing dimension a [`Threshold::RevenueShare`] reads revenue over. One variant today — the
/// customer — because that is the only dimension `list_revenue_events` can attribute without
/// guessing; it is an enum so adding `Product` later is additive rather than a wire break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdDimension {
    #[default]
    Customer,
}

impl ThresholdDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            ThresholdDimension::Customer => "customer",
        }
    }
}

/// What a rule's limit *is*: a constant, or a share of what the subject actually pays us.
///
/// `#[serde(untagged)]` with `Fixed` first: a stored `"threshold": 5.0` still reads as
/// `Fixed(5.0)`, and `{"pct": 80, "dimension": "customer"}` reads as `RevenueShare`. Order matters —
/// an object can never match `Fixed`, and a number can never match `RevenueShare`, so the two arms
/// are unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Threshold {
    Fixed(f64),
    RevenueShare {
        /// Percentage of recognized revenue, in `(0, 1000]`. Over 100 is legitimate — "you may burn
        /// up to 3x what this customer pays while we buy the land-grab" is a policy, not a typo.
        pct: f64,
        #[serde(default)]
        dimension: ThresholdDimension,
    },
}

impl Default for Threshold {
    fn default() -> Self {
        Threshold::Fixed(0.0)
    }
}

impl From<f64> for Threshold {
    fn from(v: f64) -> Self {
        Threshold::Fixed(v)
    }
}

impl Threshold {
    /// The constant, when this is one. `None` for a derived threshold — the caller must resolve it
    /// against revenue rather than substitute a number of its own.
    pub fn fixed(&self) -> Option<f64> {
        match self {
            Threshold::Fixed(v) => Some(*v),
            Threshold::RevenueShare { .. } => None,
        }
    }

    /// Validate the numeric envelope. Mirrors the old bare-`f64` check for `Fixed` exactly, so a
    /// rule that was accepted before is accepted now.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Threshold::Fixed(v) => {
                if !(v.is_finite() && *v > 0.0) {
                    return Err(format!(
                        "threshold must be a finite number greater than 0 (got {v})"
                    ));
                }
            }
            Threshold::RevenueShare { pct, .. } => {
                if !(pct.is_finite() && *pct > 0.0 && *pct <= 1000.0) {
                    return Err(format!(
                        "threshold.pct must be a finite percentage in (0, 1000] (got {pct})"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolve to the comparable number a rule evaluates against, plus the basis that explains it.
    ///
    /// `revenue_usd` is the recognized revenue the caller measured over the rule's window for the
    /// rule's subject — `None` when it could not be measured at all (no revenue rows, or a backend
    /// that does not serve them). Unknown deliberately resolves to `+inf`: a guardrail whose basis
    /// we cannot read must not start rejecting traffic on a number we invented.
    pub fn resolve(&self, revenue_usd: Option<f64>) -> (f64, ThresholdBasis) {
        match self {
            Threshold::Fixed(v) => (*v, ThresholdBasis::fixed()),
            Threshold::RevenueShare { pct, dimension } => match revenue_usd {
                Some(rev) if rev.is_finite() => (
                    rev * pct / 100.0,
                    ThresholdBasis {
                        kind: ThresholdKind::RevenueShare,
                        revenue_usd: Some(rev),
                        pct: Some(*pct),
                        dimension: Some(*dimension),
                    },
                ),
                _ => (
                    f64::INFINITY,
                    ThresholdBasis {
                        kind: ThresholdKind::Unknown,
                        revenue_usd: None,
                        pct: Some(*pct),
                        dimension: Some(*dimension),
                    },
                ),
            },
        }
    }
}

/// How a [`LimitStatus`](super::LimitStatus)' threshold was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdKind {
    #[default]
    Fixed,
    RevenueShare,
    /// A derived threshold whose basis could not be measured — the rule evaluates as `+inf` (never
    /// breaches) and this says why, instead of the status reading like a comfortable $0.00.
    Unknown,
}

impl ThresholdKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ThresholdKind::Fixed => "fixed",
            ThresholdKind::RevenueShare => "revenue_share",
            ThresholdKind::Unknown => "unknown",
        }
    }
}

/// Estimation announcing itself: the provenance of the `threshold` on a [`LimitStatus`].
///
/// A fixed cap carries nothing but its kind. A revenue-share cap carries the revenue figure and the
/// percentage it was multiplied by, so `/v1/limits/status` and the 429 message can say
/// "threshold = 80% of $412.00 recognized revenue" instead of showing a number with no story.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ThresholdBasis {
    pub kind: ThresholdKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revenue_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<ThresholdDimension>,
}

impl ThresholdBasis {
    pub fn fixed() -> Self {
        Self::default()
    }

    /// Whether the threshold rests on something we measured rather than something an operator typed.
    pub fn derived(&self) -> bool {
        !matches!(self.kind, ThresholdKind::Fixed)
    }

    /// One clause naming the basis, for a breach message or a status caveat. `None` for a plain
    /// fixed cap, which needs no explanation.
    pub fn describe(&self) -> Option<String> {
        match self.kind {
            ThresholdKind::Fixed => None,
            ThresholdKind::RevenueShare => Some(format!(
                "threshold = {:.0}% of ${:.2} recognized {} revenue",
                self.pct.unwrap_or(0.0),
                self.revenue_usd.unwrap_or(0.0),
                self.dimension.unwrap_or_default().as_str(),
            )),
            ThresholdKind::Unknown => Some(format!(
                "threshold is {:.0}% of recognized revenue, which could not be measured for this \
                 window — the rule is inert until it can be",
                self.pct.unwrap_or(0.0)
            )),
        }
    }
}

/// A temporary, forecast-driven tightening of a rule's action.
///
/// The measure→act gap this closes: `budget_breach` alerts already know a project breaches in ~2
/// days, and the only thing that ever happened with that knowledge was a Slack message. An
/// escalation says what to *do* — "when the ETA drops under 2 days, throttle for 12 hours" — and the
/// sweep applies it.
///
/// Reversal is what makes it safe: the configured [`LimitRule::action`](super::LimitRule::action) is
/// never overwritten, only shadowed until [`LimitRule::escalated_until`](super::LimitRule) passes, so
/// de-escalation is a field clear rather than a remembered undo.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Escalation {
    /// Escalate once the forecast ETA to breach is at or under this many days.
    pub on_eta_days: f64,
    /// The action to apply while escalated.
    pub to: LimitAction,
    /// How long one escalation lasts before it lapses on its own, even if nothing sweeps again.
    pub for_hours: u32,
}

impl Escalation {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.on_eta_days.is_finite() && self.on_eta_days > 0.0) {
            return Err(format!(
                "escalation.on_eta_days must be a finite number greater than 0 (got {})",
                self.on_eta_days
            ));
        }
        if self.for_hours == 0 || self.for_hours > 24 * 30 {
            return Err(format!(
                "escalation.for_hours must be between 1 and 720 (got {})",
                self.for_hours
            ));
        }
        Ok(())
    }

    /// When an escalation raised at `now` lapses.
    pub fn until(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + chrono::Duration::hours(self.for_hours as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compatibility promise: rows written before derived thresholds existed must read back
    /// with byte-identical behaviour.
    #[test]
    fn a_bare_number_deserializes_to_fixed() {
        let t: Threshold = serde_json::from_str("42.5").expect("number parses");
        assert_eq!(t, Threshold::Fixed(42.5));
        assert_eq!(serde_json::to_string(&t).unwrap(), "42.5");
    }

    #[test]
    fn an_object_deserializes_to_revenue_share_with_a_default_dimension() {
        let t: Threshold = serde_json::from_str(r#"{"pct":80}"#).expect("object parses");
        assert_eq!(
            t,
            Threshold::RevenueShare {
                pct: 80.0,
                dimension: ThresholdDimension::Customer
            }
        );
    }

    #[test]
    fn revenue_share_resolves_against_measured_revenue() {
        let t = Threshold::RevenueShare {
            pct: 80.0,
            dimension: ThresholdDimension::Customer,
        };
        let (v, basis) = t.resolve(Some(412.0));
        assert!((v - 329.6).abs() < 1e-9);
        assert_eq!(basis.kind, ThresholdKind::RevenueShare);
        assert!(basis.describe().unwrap().contains("$412.00"));
    }

    /// Unknown revenue must never invent a cap: an unmeasurable guardrail is inert, loudly.
    #[test]
    fn unknown_revenue_is_infinite_and_says_so() {
        let t = Threshold::RevenueShare {
            pct: 80.0,
            dimension: ThresholdDimension::Customer,
        };
        let (v, basis) = t.resolve(None);
        assert!(v.is_infinite());
        assert_eq!(basis.kind, ThresholdKind::Unknown);
        assert!(basis.describe().unwrap().contains("could not be measured"));
    }

    #[test]
    fn validation_matches_the_old_bare_f64_rule_and_bounds_pct() {
        assert!(Threshold::Fixed(1.0).validate().is_ok());
        assert!(Threshold::Fixed(0.0).validate().is_err());
        assert!(Threshold::Fixed(f64::NAN).validate().is_err());
        let ok = Threshold::RevenueShare {
            pct: 1000.0,
            dimension: ThresholdDimension::Customer,
        };
        assert!(ok.validate().is_ok());
        let bad = Threshold::RevenueShare {
            pct: 1000.1,
            dimension: ThresholdDimension::Customer,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn escalation_validates_its_envelope() {
        let e = Escalation {
            on_eta_days: 2.0,
            to: LimitAction::Throttle,
            for_hours: 12,
        };
        assert!(e.validate().is_ok());
        assert!(Escalation { for_hours: 0, ..e }.validate().is_err());
        assert!(Escalation {
            on_eta_days: 0.0,
            ..e
        }
        .validate()
        .is_err());
    }
}
