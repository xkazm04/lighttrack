//! Claim-level provenance for a `grounding` rubric dimension ([`crate::DimensionKind::Grounding`]).
//!
//! The fraction a grounding dimension scores is a summary; the per-claim verdict list is the audit
//! artifact. It is stored beside the dimension's reasoning so a stored score can say *which* claim
//! was unsupported, and it carries the counters that keep the fraction honest: claims issued (the
//! only denominator), verdicts that never came back, verdicts for claims nobody issued, and the
//! instruction pin that makes a score cut at one granularity distinguishable from another.

use serde::{Deserialize, Serialize};

use crate::score::cap_str;

/// Claim verdicts retained per dimension, across samples, in sample then claim order.
pub const MAX_CLAIMS_PER_DIM: usize = 32;

/// One issued claim and the verdict it received.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClaimVerdict {
    /// The judge sample that cut and verified this claim (0-based).
    pub sample: u32,
    /// The claim's number in that sample's decomposition (1-based, as the verifier saw it) — the
    /// identity its verdict was matched on, never its position in the verifier's reply.
    pub number: u32,
    pub claim: String,
    pub supported: bool,
    /// The verifier's one-sentence reason. Empty when no verdict came back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The verifier returned no verdict for this claim, so it was counted unsupported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

/// What a grounding dimension's score was computed from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroundingDetail {
    /// Pin of the decomposition and verification instructions that produced these verdicts. A
    /// different pin is a different instrument: never trend scores across a change in it.
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<ClaimVerdict>,
    /// Claims issued across every scored sample — the denominator, never the verdicts returned.
    #[serde(default)]
    pub claims_issued: u32,
    /// Issued claims that received no verdict; each was counted unsupported.
    #[serde(default)]
    pub missing_verdicts: u32,
    /// Verdicts naming a claim that was never issued (or naming one twice); ignored in the score.
    #[serde(default)]
    pub stray_verdicts: u32,
    /// Samples whose output yielded zero claims, and so scored nothing on this dimension.
    #[serde(default)]
    pub unscored_samples: u32,
}

impl GroundingDetail {
    /// Enforce the storage bounds, the same way [`crate::ScoreDetail::capped`] bounds reasoning.
    pub fn capped(mut self) -> Self {
        self.claims.truncate(MAX_CLAIMS_PER_DIM);
        for c in &mut self.claims {
            c.claim = cap_str(&c.claim);
            c.reason = cap_str(&c.reason);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capping_bounds_the_claim_list_and_each_string() {
        let long = "c".repeat(crate::MAX_REASONING_CHARS + 10);
        let d = GroundingDetail {
            version: "v".into(),
            claims: (0..MAX_CLAIMS_PER_DIM + 5)
                .map(|i| ClaimVerdict {
                    number: i as u32 + 1,
                    claim: long.clone(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
        .capped();
        assert_eq!(d.claims.len(), MAX_CLAIMS_PER_DIM);
        assert!(d.claims[0].claim.ends_with('…'));
    }
}
