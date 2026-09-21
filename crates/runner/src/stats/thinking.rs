//! What a target spent **thinking**, per case and per difficulty tier.
//!
//! Difficulty grades come from an operator, and the live 2026-09-07 run showed how far that can miss:
//! a `medium` tier of classic cognitive-reflection traps was scored 1.00 by every target at every
//! effort, because those puzzles are famous. Thinking tokens are the run's *own* measure of where a
//! model found work to do — a tier where thinking does not rise is a tier this corpus did not make
//! harder for this model.
//!
//! It is reported **beside** the operator's grades and never in place of them, and it is never
//! derived from the scores: deriving difficulty from outcomes is the circularity D24/D25 refused.

use serde_json::{json, Value};

use lighttrack_core::Difficulty;

use super::tiers::{grade_of, ladder, Tier};
use crate::case_spend::ThinkingBasis;

/// One judged case as the thinking measures see it: what it scored, whether it passed, what its
/// generation spent. Case-identified, because every pairing in this module is by case id.
#[derive(Debug, Clone)]
pub(crate) struct CaseSpend {
    pub(crate) case: u32,
    pub(crate) score: f64,
    pub(crate) pass: bool,
    /// Mean output tokens per candidate (reasoning included — it is billed).
    pub(crate) output_tokens: Option<f64>,
    /// Mean hidden-reasoning tokens per candidate, where the provider reported the split.
    pub(crate) reasoning_tokens: Option<f64>,
    /// Mean generation cost per candidate, USD. `None` when the model was unpriced.
    pub(crate) cost_usd: Option<f64>,
}

impl CaseSpend {
    /// This case's thinking on the given basis. `None` when that basis was not measured here — which
    /// is what keeps a missing count out of a median instead of entering it as zero.
    pub(crate) fn thinking(&self, basis: ThinkingBasis) -> Option<f64> {
        match basis {
            ThinkingBasis::Reasoning => self.reasoning_tokens,
            ThinkingBasis::Output => self.output_tokens,
        }
    }
}

/// The median of a sample, or `None` when it is empty. Median rather than mean on purpose: one case
/// where a model spiralled to its token cap would drag a mean far past what it typically spends.
pub(crate) fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len().max(1) as f64
}

/// Layer `thinking_by_tier` onto **one target's** run report: the median thinking tokens and mean
/// score per difficulty bucket, in ladder order with `ungraded` last.
///
/// Additive and omitted entirely when there is nothing to say — no basis measured, or a corpus with
/// no grades — so a run that could not measure thinking looks exactly as it always did.
pub(crate) fn annotate_thinking_tiers(
    report: &mut Value,
    cases: &[CaseSpend],
    tiers: &[Option<Difficulty>],
    basis: Option<ThinkingBasis>,
) {
    let Some(basis) = basis else {
        return;
    };
    if !cases.iter().any(|c| grade_of(tiers, c.case).is_some()) {
        return;
    }
    let rows: Vec<Value> = ladder()
        .into_iter()
        .filter_map(|t| tier_row(t, cases, tiers, basis))
        .collect();
    if rows.is_empty() {
        return;
    }
    if let Some(obj) = report.as_object_mut() {
        obj.insert(
            "thinking_by_tier".into(),
            json!({ "basis": basis.as_str(), "tiers": rows }),
        );
    }
}

fn tier_row(
    t: Tier,
    cases: &[CaseSpend],
    tiers: &[Option<Difficulty>],
    basis: ThinkingBasis,
) -> Option<Value> {
    let here: Vec<&CaseSpend> = cases
        .iter()
        .filter(|c| t.holds(grade_of(tiers, c.case)))
        .collect();
    if here.is_empty() {
        return None;
    }
    let thinking: Vec<f64> = here.iter().filter_map(|c| c.thinking(basis)).collect();
    let scores: Vec<f64> = here.iter().map(|c| c.score).collect();
    Some(json!({
        "tier": t.as_str(),
        "n_cases": here.len(),
        // `null` when this tier's cases reported no count at all — never 0, which would read as a
        // tier the model answered without thinking.
        "median_thinking_tokens": median(&thinking).map(round1),
        // How many of the tier's cases actually carried a count, so a median over 2 of 9 says so.
        "measured_cases": thinking.len(),
        "mean_score": (mean(&scores) * 1000.0).round() / 1000.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn case(case: u32, score: f64, thinking: Option<f64>) -> CaseSpend {
        CaseSpend {
            case,
            score,
            pass: score >= 0.5,
            output_tokens: thinking,
            reasoning_tokens: thinking,
            cost_usd: Some(0.01),
        }
    }

    fn graded(spec: &[&str]) -> Vec<Option<Difficulty>> {
        spec.iter().map(|s| Difficulty::parse(s)).collect()
    }

    #[test]
    fn medians_resist_the_one_case_that_spiralled() {
        assert_eq!(median(&[100.0, 120.0, 110.0]), Some(110.0));
        // A mean would read 4077 here; the median says what the model typically spends.
        assert_eq!(median(&[100.0, 120.0, 110.0, 16000.0]), Some(115.0));
        assert_eq!(median(&[]), None);
    }

    /// **The measure the live run had to compute by hand.** Thinking rises with the tier — and the
    /// table says so per rung, beside the operator's grades rather than instead of them.
    #[test]
    fn thinking_is_reported_per_tier_in_ladder_order() {
        let tiers = graded(&["easy", "easy", "hard", "hard", ""]);
        let cases = vec![
            case(1, 1.0, Some(120.0)),
            case(2, 1.0, Some(140.0)),
            case(3, 0.5, Some(3400.0)),
            case(4, 1.0, Some(3600.0)),
            case(5, 1.0, Some(200.0)),
        ];
        let mut report = json!({ "mode": "compare" });
        annotate_thinking_tiers(&mut report, &cases, &tiers, Some(ThinkingBasis::Reasoning));
        let block = &report["thinking_by_tier"];
        assert_eq!(block["basis"], json!("reasoning_tokens"));
        let names: Vec<&str> = block["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tier"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["easy", "hard", "ungraded"]);
        assert_eq!(block["tiers"][0]["median_thinking_tokens"], json!(130.0));
        assert_eq!(block["tiers"][1]["median_thinking_tokens"], json!(3500.0));
        assert_eq!(block["tiers"][0]["n_cases"], json!(2));
    }

    /// A tier whose cases reported no count reads as `null`, never as a model that answered without
    /// thinking — and the measured count travels so a median over a fraction of the tier says so.
    #[test]
    fn an_unmeasured_tier_is_null_not_zero() {
        let tiers = graded(&["hard", "hard"]);
        let cases = vec![case(1, 1.0, None), case(2, 1.0, Some(500.0))];
        let mut report = json!({});
        annotate_thinking_tiers(&mut report, &cases, &tiers, Some(ThinkingBasis::Output));
        let row = &report["thinking_by_tier"]["tiers"][0];
        assert_eq!(row["median_thinking_tokens"], json!(500.0));
        assert_eq!(row["measured_cases"], json!(1), "1 of the 2 cases");
        assert_eq!(row["n_cases"], json!(2));

        let mut none = json!({});
        annotate_thinking_tiers(
            &mut none,
            &[case(1, 1.0, None)],
            &graded(&["hard"]),
            Some(ThinkingBasis::Output),
        );
        assert_eq!(
            none["thinking_by_tier"]["tiers"][0]["median_thinking_tokens"],
            Value::Null
        );
    }

    /// No basis measured, or no grades at all: no block, exactly as every run before this existed.
    #[test]
    fn nothing_to_say_produces_no_block() {
        let mut a = json!({});
        annotate_thinking_tiers(&mut a, &[case(1, 1.0, Some(9.0))], &graded(&["easy"]), None);
        assert!(a.get("thinking_by_tier").is_none());

        let mut b = json!({});
        annotate_thinking_tiers(
            &mut b,
            &[case(1, 1.0, Some(9.0))],
            &[None],
            Some(ThinkingBasis::Output),
        );
        assert!(b.get("thinking_by_tier").is_none());
    }
}
