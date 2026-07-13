//! Pairwise preference judging with position debias. Each pair of answers is judged twice with the
//! A/B order swapped; the verdict is the *agreement* of the two orders, and any disagreement (order
//! flipped the winner) collapses to a Tie and flags position bias. This is the standard swap-and-
//! average fix for "which model/prompt do I ship?" — content preference, not per-dimension scoring.

use serde::Deserialize;

use crate::parse::{extract_json_object, sample_parsed, Parsed};
use crate::prompts::{build_pairwise_prompt, build_pairwise_schema};
use crate::providers::generate;
use crate::{EngineConfig, EngineError, GenOutcome, Result};

/// Which answer a pairwise judgement preferred. `A`/`B` are in the *caller's* order (A = first arg).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum PairwiseWinner {
    #[serde(rename = "A", alias = "a")]
    A,
    #[serde(rename = "B", alias = "b")]
    B,
    #[serde(rename = "Tie", alias = "tie", alias = "TIE", alias = "tie ")]
    Tie,
}

/// The structured verdict a pairwise judge returns for one A-vs-B call.
#[derive(Debug, Clone, Deserialize)]
pub struct PairwiseVerdict {
    pub winner: PairwiseWinner,
    #[serde(default)]
    pub reasoning: String,
}

/// The debiased result of judging one pair twice (orders swapped).
#[derive(Debug, Clone)]
pub struct PairwiseOutcome {
    /// The agreed winner in the caller's order (A = first answer). `Tie` if the two orders disagreed.
    pub winner: PairwiseWinner,
    /// True when swapping the order flipped the winner — a detected position bias, forced to `Tie`.
    pub position_bias: bool,
    pub reasoning: String,
    pub cost_usd: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tokens: u64,
    pub model: String,
}

/// Map a second-order verdict (where the judge saw the answers swapped) back to the caller's order.
fn unswap(w: PairwiseWinner) -> PairwiseWinner {
    match w {
        PairwiseWinner::A => PairwiseWinner::B,
        PairwiseWinner::B => PairwiseWinner::A,
        PairwiseWinner::Tie => PairwiseWinner::Tie,
    }
}

/// Combine the two orders: agree ⇒ that winner; disagree (including one Tie) ⇒ Tie + bias flag.
fn combine(order1: PairwiseWinner, order2_raw: PairwiseWinner) -> (PairwiseWinner, bool) {
    let order2 = unswap(order2_raw);
    if order1 == order2 {
        (order1, false)
    } else {
        (PairwiseWinner::Tie, true)
    }
}

fn add_cost(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        _ => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

fn parse_pairwise(raw: &str) -> Result<PairwiseVerdict> {
    let json = extract_json_object(raw)
        .ok_or_else(|| EngineError::Parse(format!("no JSON object in pairwise output: {raw}")))?;
    serde_json::from_str::<PairwiseVerdict>(&json)
        .map_err(|e| EngineError::Parse(format!("pairwise JSON not a verdict: {e}; got: {json}")))
}

/// Assemble the debiased outcome from the two per-order parses, summing cost/tokens across both calls.
fn assemble(r1: Parsed<PairwiseVerdict>, r2: Parsed<PairwiseVerdict>) -> Result<PairwiseOutcome> {
    let v1 = r1
        .value
        .ok_or_else(|| EngineError::Parse("pairwise judge produced no verdict (order 1)".into()))?;
    let v2 = r2
        .value
        .ok_or_else(|| EngineError::Parse("pairwise judge produced no verdict (order 2)".into()))?;
    let (winner, position_bias) = combine(v1.winner, v2.winner);
    let reasoning = if position_bias {
        format!("orders disagreed (position bias) — order1: {}; order2: {}", v1.reasoning, v2.reasoning)
    } else {
        v1.reasoning.clone()
    };
    let input_tokens = r1.input_tokens + r2.input_tokens;
    let output_tokens = r1.output_tokens + r2.output_tokens;
    let model = if !r2.model.is_empty() { r2.model } else { r1.model };
    Ok(PairwiseOutcome {
        winner,
        position_bias,
        reasoning,
        cost_usd: add_cost(r1.cost_usd, r2.cost_usd),
        input_tokens,
        output_tokens,
        tokens: input_tokens + output_tokens,
        model,
    })
}

/// Judge one pair via `gen` (a single already-retried generation for `(order, prompt)`), swapping the
/// A/B order between the two calls and combining them. Split from [`run_pairwise`] so a fake generator
/// can drive the swap/parse/combine path without live calls.
fn pairwise_via(
    gen: impl Fn(usize, &str) -> Result<GenOutcome>,
    input: &str,
    expected: Option<&str>,
    answer_a: &str,
    answer_b: &str,
    criteria: Option<&str>,
) -> Result<PairwiseOutcome> {
    let p1 = build_pairwise_prompt(input, expected, answer_a, answer_b, criteria);
    let p2 = build_pairwise_prompt(input, expected, answer_b, answer_a, criteria);
    let r1 = sample_parsed(&gen, 0, &p1, parse_pairwise)?;
    let r2 = sample_parsed(&gen, 1, &p2, parse_pairwise)?;
    assemble(r1, r2)
}

/// Judge which of two answers is better for `input` (or a tie), debiased by swapping the A/B order and
/// requiring both orders to agree. Uses the schema-enforced, repair-backed generation path.
#[allow(clippy::too_many_arguments)]
pub fn run_pairwise(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    input: &str,
    expected: Option<&str>,
    answer_a: &str,
    answer_b: &str,
    criteria: Option<&str>,
) -> Result<PairwiseOutcome> {
    let schema = build_pairwise_schema();
    pairwise_via(
        |_order, p| generate(cfg, provider, model, None, p, Some(&schema)),
        input,
        expected,
        answer_a,
        answer_b,
        criteria,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unswap_inverts_a_and_b_keeps_tie() {
        assert_eq!(unswap(PairwiseWinner::A), PairwiseWinner::B);
        assert_eq!(unswap(PairwiseWinner::B), PairwiseWinner::A);
        assert_eq!(unswap(PairwiseWinner::Tie), PairwiseWinner::Tie);
    }

    #[test]
    fn combine_agreement_and_disagreement() {
        // Both orders prefer the caller's A (order2 says "B" because it saw them swapped).
        assert_eq!(combine(PairwiseWinner::A, PairwiseWinner::B), (PairwiseWinner::A, false));
        // Both prefer caller's B.
        assert_eq!(combine(PairwiseWinner::B, PairwiseWinner::A), (PairwiseWinner::B, false));
        // Both say Tie.
        assert_eq!(combine(PairwiseWinner::Tie, PairwiseWinner::Tie), (PairwiseWinner::Tie, false));
        // Order flipped the winner (both raw "A") => position bias => Tie.
        assert_eq!(combine(PairwiseWinner::A, PairwiseWinner::A), (PairwiseWinner::Tie, true));
        // One order is decisive, the other a tie => disagreement => Tie.
        assert_eq!(combine(PairwiseWinner::A, PairwiseWinner::Tie), (PairwiseWinner::Tie, true));
    }

    /// Drive the full pairwise path with a fake generator keyed on the order index (0 = A-first,
    /// 1 = B-first), proving swap handling, cost summation, and tie/bias resolution with no live calls.
    fn fake(order0: &'static str, order1: &'static str) -> impl Fn(usize, &str) -> Result<GenOutcome> {
        move |order, _p| {
            let output = if order == 0 { order0 } else { order1 }.to_string();
            Ok(GenOutcome {
                output,
                cost_usd: Some(0.01),
                model: "fake".into(),
                latency_ms: Some(1),
                input_tokens: Some(3),
                output_tokens: Some(2),
            })
        }
    }

    #[test]
    fn consistent_winner_survives_the_swap() {
        // order0 (A=answer_a) says A wins; order1 (A=answer_b) says B wins ⇒ both prefer answer_a.
        let out = pairwise_via(
            fake(r#"{"winner":"A","reasoning":"a is correct"}"#, r#"{"winner":"B","reasoning":"a is correct"}"#),
            "q", None, "answer_a", "answer_b", Some("accuracy"),
        )
        .unwrap();
        assert_eq!(out.winner, PairwiseWinner::A);
        assert!(!out.position_bias);
        assert_eq!(out.cost_usd, Some(0.02), "both calls' cost summed");
        assert_eq!(out.tokens, 10);
    }

    #[test]
    fn positional_disagreement_becomes_tie() {
        // Both orders name the *first-shown* answer ("A") ⇒ the judge just favors position ⇒ Tie+bias.
        let out = pairwise_via(
            fake(r#"{"winner":"A","reasoning":"x"}"#, r#"{"winner":"A","reasoning":"y"}"#),
            "q", None, "answer_a", "answer_b", None,
        )
        .unwrap();
        assert_eq!(out.winner, PairwiseWinner::Tie);
        assert!(out.position_bias, "flipped winner must flag position bias");
    }
}
