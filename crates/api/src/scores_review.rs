//! `GET /v1/scores?needs_review=1` — the verdicts a human should look at first (M11).
//!
//! Every signal here already existed on the row and none of it was reachable as a question. A judge
//! that flagged an injection, split across its own samples, landed a hair from the pass threshold,
//! or was flatly contradicted by the human who graded the same subject is exactly the verdict worth
//! a person's attention — and until now finding those meant reading `detail` blobs by hand.
//!
//! The triage is a pure function of `(score, threshold, label)` so it is testable without a router,
//! and so the reasons it gives are the same ones the render layer shows.

use lighttrack_core::{Label, Score};

/// Cross-sample agreement at or below this means the judge disagreed with itself.
pub(crate) const LOW_AGREEMENT: f64 = 0.7;
/// Within this of the pass threshold, a verdict's pass/fail is a coin toss the judge happened to
/// win — which is where a re-run flips the gate and nobody knows why.
pub(crate) const NEAR_THRESHOLD: f64 = 0.05;
/// A judge and a human this far apart on the same subject are not measuring the same thing.
pub(crate) const DISAGREEMENT: f64 = 0.25;

/// Why this verdict wants a human. Empty ⇒ it does not.
///
/// Ordered most-decisive first, so a caller that shows only one reason shows the one that matters:
/// a human contradiction beats a self-contradiction beats a boundary.
pub(crate) fn review_reasons(
    s: &Score,
    threshold: f64,
    label: Option<&Label>,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(l) = label {
        // Normalize the judge onto the label's 0..1 before comparing: `max` is the judge's scale and
        // a label has none, so comparing raw values would flag every 0..5 rubric as a disagreement.
        let judged = normalized(s);
        if (judged - l.value).abs() >= DISAGREEMENT {
            out.push("human_disagreement");
        } else if s.pass.is_some() && s.pass != Some(l.passed(threshold)) {
            // A close-but-opposite call: the numbers nearly agree and the verdicts do not, which is
            // a threshold problem rather than a judging one — and is worth saying separately.
            out.push("human_pass_mismatch");
        }
    }
    if let Some(d) = &s.detail {
        if d.injection_suspected == Some(true) {
            out.push("injection_suspected");
        }
        if d.agreement.is_some_and(|a| a <= LOW_AGREEMENT) {
            out.push("low_agreement");
        }
        if d.dimensions.iter().any(|dim| dim.floor_hit) {
            out.push("floor_hit");
        }
        if d.position_bias == Some(true) {
            out.push("position_bias");
        }
        if d.parse_failures.is_some_and(|n| n > 0) {
            out.push("parse_failures");
        }
    }
    if (normalized(s) - threshold).abs() <= NEAR_THRESHOLD {
        out.push("near_threshold");
    }
    out
}

/// The verdict on 0..1. A `max` of zero (or worse) would make the ratio meaningless, so it falls
/// back to the raw value rather than producing an infinity that flags everything.
fn normalized(s: &Score) -> f64 {
    if s.max > 0.0 {
        s.value / s.max
    } else {
        s.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lighttrack_core::{LabelSubject, ScoreDetail, ScoreDim, ScoreKind};

    fn score(value: f64, detail: Option<ScoreDetail>) -> Score {
        Score {
            id: "s".into(),
            project_id: "p".into(),
            event_id: Some("e".into()),
            rubric: "quality".into(),
            rubric_id: None,
            kind: ScoreKind::Freeform,
            value,
            max: 1.0,
            pass: Some(value >= 0.7),
            reasoning: None,
            detail,
            run_id: None,
            case_index: None,
            scored_by: "haiku".into(),
            cost_usd: None,
            created_at: Utc::now(),
        }
    }

    fn label(value: f64) -> Label {
        Label {
            id: "l".into(),
            project_id: "p".into(),
            subject: LabelSubject::Score("s".into()),
            rubric_id: None,
            value,
            pass: None,
            dimensions: vec![],
            labeler: "me".into(),
            note: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn a_confident_verdict_far_from_the_threshold_needs_nothing() {
        assert!(review_reasons(&score(0.95, None), 0.7, None).is_empty());
    }

    #[test]
    fn a_verdict_a_human_contradicts_is_the_first_reason_given() {
        let r = review_reasons(&score(0.95, None), 0.7, Some(&label(0.2)));
        assert_eq!(r.first(), Some(&"human_disagreement"), "{r:?}");
    }

    /// Near-agreement with an opposite pass/fail is its own finding: the judging is fine and the
    /// threshold is in the wrong place.
    #[test]
    fn agreeing_numbers_with_opposite_calls_report_the_threshold_not_the_judge() {
        let r = review_reasons(&score(0.71, None), 0.7, Some(&label(0.69)));
        assert!(r.contains(&"human_pass_mismatch"), "{r:?}");
        assert!(!r.contains(&"human_disagreement"), "{r:?}");
    }

    #[test]
    fn the_detail_flags_each_surface_their_own_reason() {
        let d = ScoreDetail {
            agreement: Some(0.4),
            injection_suspected: Some(true),
            position_bias: Some(true),
            parse_failures: Some(2),
            dimensions: vec![ScoreDim {
                key: "safety".into(),
                value: 0.1,
                weight: 1.0,
                floor: Some(0.5),
                floor_hit: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let r = review_reasons(&score(0.95, Some(d)), 0.7, None);
        for want in [
            "injection_suspected",
            "low_agreement",
            "floor_hit",
            "position_bias",
            "parse_failures",
        ] {
            assert!(r.contains(&want), "missing {want} in {r:?}");
        }
    }

    /// A verdict sitting on the threshold is a coin toss the judge happened to win — a re-run flips
    /// the gate and nobody knows why.
    #[test]
    fn a_verdict_on_the_boundary_is_flagged_on_both_sides() {
        assert!(review_reasons(&score(0.72, None), 0.7, None).contains(&"near_threshold"));
        assert!(review_reasons(&score(0.68, None), 0.7, None).contains(&"near_threshold"));
        assert!(!review_reasons(&score(0.5, None), 0.7, None).contains(&"near_threshold"));
    }

    /// A rubric scored out of 5 must not read as a disagreement with every 0..1 label.
    #[test]
    fn the_judges_scale_is_normalized_before_it_is_compared_with_a_human() {
        let mut s = score(4.5, None);
        s.max = 5.0;
        s.pass = Some(true);
        assert!(review_reasons(&s, 0.7, Some(&label(0.9))).is_empty());
    }
}
