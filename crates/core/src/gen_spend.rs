//! What generating a judged case's candidates cost — the per-case half of a compare run's spend.
//!
//! A compare run used to keep generation tokens and latency only as per-target sums and percentiles,
//! so "effort helped on case 7 because the model thought nine times longer there" could not be read
//! back from anything it stored. These facts ride on the verdict's [`ScoreDetail`](crate::ScoreDetail),
//! which is persisted once per case, rather than only on the run report's bounded case preview.

use serde::{Deserialize, Serialize};

/// Per-candidate means for one judged case. Every figure is a **mean over the candidates that
/// generated**, so a `--gen-samples 3` cell and a single-draw cell read on the same scale.
///
/// `None` is always "not known", never zero: a model with no price is not a free call, and a
/// provider that reports no reasoning split is not a model that did not think.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GenerationFacts {
    /// Candidates that generated for this case.
    pub candidates: u32,
    /// Output tokens per candidate — hidden reasoning included, because that is what is billed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<f64>,
    /// The hidden-reasoning share of `output_tokens`, where the provider reports the split
    /// (OpenAI, Gemini, OpenRouter). The Anthropic Messages API and `claude -p` report none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    /// Generation cost per candidate, USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Per-case limits this case exceeded (`cost`, `latency`) — the reason a well-scored case can
    /// still be a failure. See [`crate::case_limits`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limit_breaches: Vec<String>,
    /// Limits that were set but could not be checked here — an unpriced model has no cost to
    /// compare. Recorded, because a limit that silently stopped gating is worth more to know about
    /// than one that passed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits_unchecked: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScoreDetail;
    use serde_json::json;

    /// Every stored verdict predates this field, and must keep reading back exactly as it was.
    #[test]
    fn a_detail_written_before_generation_facts_still_reads() {
        let d: ScoreDetail = serde_json::from_value(json!({ "agreement": 0.5 })).unwrap();
        assert!(d.generation.is_none());
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({ "agreement": 0.5 })
        );
    }

    /// Unknowns are absent keys, not zeroes — a reader must never see `cost_usd: 0` for an unpriced
    /// model or `reasoning_tokens: 0` for a provider that gave no split.
    #[test]
    fn unknown_figures_are_omitted_not_zeroed() {
        let f = GenerationFacts {
            candidates: 2,
            output_tokens: Some(812.5),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            json!({ "candidates": 2, "output_tokens": 812.5 })
        );
        let back: GenerationFacts =
            serde_json::from_value(json!({ "candidates": 2, "output_tokens": 812.5 })).unwrap();
        assert_eq!(back, f);
    }
}
