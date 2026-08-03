//! Turning an engine judge outcome into the [`ScoreDetail`] that ships with a posted score.
//!
//! Judge runs compute per-dimension scores, per-sample reasoning, agreement and parse-failure
//! counts. Before this module, almost none of it was persisted: a `Score` row carried a scalar and a
//! templated string, so the reasoning tokens the run paid for were deleted and no stored verdict
//! could answer "why did this score happen?". Everything here is pure mapping — no I/O — so it is
//! unit-tested without a judge.

use lighttrack_core::{ScoreDetail, ScoreDim};
use lighttrack_engine::{JudgeOutcome, RubricOutcome};

/// Structured provenance for one rubric verdict: every dimension, every sample's reasoning, and the
/// reliability counters behind the scalar.
pub(crate) fn rubric_detail(o: &RubricOutcome) -> ScoreDetail {
    ScoreDetail {
        dimensions: o
            .dimensions
            .iter()
            .map(|d| ScoreDim {
                key: d.key.clone(),
                value: d.score,
                weight: d.weight,
                floor: d.floor,
                floor_hit: d.floor_hit,
                reasoning: d.reasonings.clone(),
            })
            .collect(),
        agreement: Some(o.agreement),
        samples_requested: Some(o.samples),
        samples_parsed: Some(o.samples_parsed),
        parse_failures: Some(o.parse_failures),
        position_bias: None,
        injection_suspected: Some(o.injection_suspected),
        notes: Vec::new(),
    }
    .capped()
}

/// Provenance for a freeform (non-rubric) verdict: no dimensions, but the judge's own rationale and
/// the injection signal are still worth keeping.
pub(crate) fn freeform_detail(o: &JudgeOutcome) -> ScoreDetail {
    let notes = if o.verdict.reasoning.is_empty() {
        Vec::new()
    } else {
        vec![o.verdict.reasoning.clone()]
    };
    ScoreDetail {
        samples_requested: Some(1),
        samples_parsed: Some(1),
        parse_failures: Some(0),
        injection_suspected: Some(o.injection_suspected),
        notes,
        ..Default::default()
    }
    .capped()
}

/// A one-line human summary drawn from the judge's *own words*: the weakest dimension's first
/// reasoning. Never a template — a `Score.reasoning` that says "rubric 'x' overall over 4 dims"
/// tells a reader nothing they couldn't compute. `None` when the judge returned no prose.
pub(crate) fn weakest_reasoning(detail: &ScoreDetail) -> Option<String> {
    let weakest = detail
        .dimensions
        .iter()
        .filter(|d| !d.reasoning.is_empty())
        .min_by(|a, b| a.value.total_cmp(&b.value))?;
    Some(format!("{}: {}", weakest.key, weakest.reasoning[0]))
}

/// Merge the per-candidate details of one comparison cell (compare mode judges `gen_samples`
/// candidates per case and reports their mean). Dimension values average; every candidate's
/// reasoning is kept, in candidate order; counters sum; flags OR. First-seen key order, so the merge
/// is deterministic at any `--jobs`.
pub(crate) fn merge_details(details: &[ScoreDetail]) -> ScoreDetail {
    let mut keys: Vec<String> = Vec::new();
    let mut acc: Vec<ScoreDim> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut out = ScoreDetail::default();
    let (mut agree_sum, mut agree_n) = (0.0_f64, 0_u32);

    for d in details {
        for dim in &d.dimensions {
            match keys.iter().position(|k| k == &dim.key) {
                Some(i) => {
                    acc[i].value += dim.value;
                    acc[i].floor_hit |= dim.floor_hit;
                    acc[i].reasoning.extend(dim.reasoning.iter().cloned());
                    counts[i] += 1;
                }
                None => {
                    keys.push(dim.key.clone());
                    acc.push(dim.clone());
                    counts.push(1);
                }
            }
        }
        if let Some(a) = d.agreement {
            agree_sum += a;
            agree_n += 1;
        }
        out.samples_requested = add(out.samples_requested, d.samples_requested);
        out.samples_parsed = add(out.samples_parsed, d.samples_parsed);
        out.parse_failures = add(out.parse_failures, d.parse_failures);
        out.position_bias = or(out.position_bias, d.position_bias);
        out.injection_suspected = or(out.injection_suspected, d.injection_suspected);
        out.notes.extend(d.notes.iter().cloned());
    }
    for (dim, n) in acc.iter_mut().zip(&counts) {
        dim.value /= *n as f64;
    }
    out.dimensions = acc;
    out.agreement = (agree_n > 0).then(|| agree_sum / agree_n as f64);
    out.capped()
}

fn add(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        _ => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (None, None) => None,
        _ => Some(a.unwrap_or(false) || b.unwrap_or(false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dim(key: &str, value: f64, reasoning: &[&str]) -> ScoreDim {
        ScoreDim {
            key: key.into(),
            value,
            weight: 1.0,
            floor: None,
            floor_hit: false,
            reasoning: reasoning.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn detail(dims: Vec<ScoreDim>, agreement: f64) -> ScoreDetail {
        ScoreDetail {
            dimensions: dims,
            agreement: Some(agreement),
            samples_requested: Some(2),
            samples_parsed: Some(2),
            parse_failures: Some(0),
            injection_suspected: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn weakest_reasoning_quotes_the_judge_not_a_template() {
        let d = detail(
            vec![dim("correctness", 0.9, &["nails it"]), dim("safety", 0.2, &["unsafe advice"])],
            1.0,
        );
        assert_eq!(weakest_reasoning(&d).as_deref(), Some("safety: unsafe advice"));
    }

    #[test]
    fn weakest_reasoning_is_none_when_the_judge_wrote_nothing() {
        assert!(weakest_reasoning(&detail(vec![dim("x", 0.5, &[])], 1.0)).is_none());
    }

    #[test]
    fn merge_averages_values_and_keeps_every_candidates_reasoning() {
        let a = detail(vec![dim("x", 0.8, &["a1", "a2"]), dim("y", 0.4, &["ay"])], 1.0);
        let b = detail(vec![dim("x", 0.4, &["b1"]), dim("y", 0.6, &["by"])], 0.6);
        let m = merge_details(&[a, b]);
        assert_eq!(m.dimensions.len(), 2);
        assert_eq!(m.dimensions[0].key, "x", "first-seen key order");
        assert!((m.dimensions[0].value - 0.6).abs() < 1e-9, "mean of 0.8 and 0.4");
        assert_eq!(m.dimensions[0].reasoning, vec!["a1", "a2", "b1"], "nothing discarded");
        assert!((m.agreement.unwrap() - 0.8).abs() < 1e-9, "agreement averages");
        assert_eq!(m.samples_requested, Some(4), "sample counters sum");
        assert_eq!(m.injection_suspected, Some(false));
    }

    #[test]
    fn merge_ors_the_flags_and_is_empty_safe() {
        assert!(merge_details(&[]).is_empty());
        let clean = detail(vec![dim("x", 1.0, &["fine"])], 1.0);
        let mut dirty = detail(vec![dim("x", 0.0, &["spoofed"])], 1.0);
        dirty.injection_suspected = Some(true);
        dirty.position_bias = Some(true);
        let m = merge_details(&[clean, dirty]);
        assert_eq!(m.injection_suspected, Some(true), "one dirty candidate taints the cell");
        assert_eq!(m.position_bias, Some(true));
    }

    #[test]
    fn capping_bounds_a_hot_score_row() {
        let long = "x".repeat(lighttrack_core::MAX_REASONING_CHARS + 500);
        let many: Vec<String> = (0..40).map(|_| long.clone()).collect();
        let d = ScoreDetail {
            dimensions: vec![ScoreDim {
                key: "x".into(),
                value: 0.5,
                weight: 1.0,
                floor: None,
                floor_hit: false,
                reasoning: many,
            }],
            ..Default::default()
        };
        let capped = merge_details(&[d]);
        let r = &capped.dimensions[0].reasoning;
        assert_eq!(r.len(), lighttrack_core::MAX_REASONINGS_PER_DIM);
        assert_eq!(r[0].chars().count(), lighttrack_core::MAX_REASONING_CHARS);
        assert!(r[0].ends_with('…'), "truncation is visible to a reader");
    }
}
