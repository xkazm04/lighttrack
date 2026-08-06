//! Unit tests for batched judging, driven through the [`Generator`] seam with deterministic fakes.
//!
//! The interesting cases are all about **attribution**: a batched response is only safe because a
//! verdict is matched to its case by an echoed id. These tests pin the ways that can go wrong —
//! dropped, reordered, duplicated and invented entries — because the failure they prevent (every
//! score after a gap sliding onto the wrong candidate) is silent, plausible, and would never be
//! caught downstream.

use serde_json::{json, Value};

use lighttrack_core::Rubric;

use crate::{Determinism, GenOutcome, Result};

use super::*;

fn rubric(json: Value) -> Rubric {
    serde_json::from_value(json).unwrap()
}

/// A one-LLM-dimension rubric — enough to exercise attribution without noise from the scoring math,
/// which `judge::tests` already covers.
fn simple_rubric() -> Rubric {
    rubric(json!({
        "name": "r",
        "threshold": 0.5,
        "dimensions": [{ "key": "correctness", "description": "is it right", "weight": 1.0 }]
    }))
}

fn cases<'a>(outputs: &'a [&'a str]) -> Vec<BatchCase<'a>> {
    outputs
        .iter()
        .map(|o| BatchCase {
            input: "q",
            expected: None,
            output: o,
        })
        .collect()
}

fn gen_outcome(output: String) -> GenOutcome {
    GenOutcome {
        output,
        cost_usd: Some(0.10),
        model: "fake".into(),
        latency_ms: Some(400),
        input_tokens: Some(1000),
        output_tokens: Some(100),
        determinism: Determinism::Exact,
    }
}

/// Replays one canned batched response per sample index (cycling).
struct FakeBatch {
    responses: Vec<String>,
}

impl Generator for FakeBatch {
    fn generate(&self, index: usize, _prompt: &str) -> Result<GenOutcome> {
        Ok(gen_outcome(
            self.responses[index % self.responses.len()].clone(),
        ))
    }
}

/// A verdict entry for `case-<i>` scoring `score`.
fn verdict(i: usize, score: f64) -> Value {
    json!({ "case_id": format!("case-{i}"), "correctness": { "score": score, "reasoning": "r" } })
}

fn response(entries: Vec<Value>) -> String {
    json!({ "verdicts": entries }).to_string()
}

fn run(
    r: &Rubric,
    cs: &[BatchCase<'_>],
    responses: &[&str],
    samples: u32,
) -> Vec<Result<RubricOutcome>> {
    let gen = FakeBatch {
        responses: responses.iter().map(|s| s.to_string()).collect(),
    };
    batch_with(&gen, r, cs, "fake", samples, 1).expect("batch call itself should succeed")
}

#[test]
fn each_case_gets_its_own_verdict() {
    let r = simple_rubric();
    let cs = cases(&["a", "b", "c"]);
    let body = response(vec![verdict(0, 1.0), verdict(1, 0.5), verdict(2, 0.0)]);
    let out = run(&r, &cs, &[&body], 1);

    let scores: Vec<f64> = out.iter().map(|o| o.as_ref().unwrap().overall).collect();
    assert_eq!(scores, vec![1.0, 0.5, 0.0]);
}

/// The load-bearing one. A response whose entries arrive in a different order than the cases were
/// presented must still land on the right cases — position must never be consulted.
#[test]
fn verdicts_are_matched_by_id_not_position() {
    let r = simple_rubric();
    let cs = cases(&["a", "b", "c"]);
    // Deliberately reversed relative to the case order.
    let body = response(vec![verdict(2, 0.0), verdict(1, 0.5), verdict(0, 1.0)]);
    let out = run(&r, &cs, &[&body], 1);

    let scores: Vec<f64> = out.iter().map(|o| o.as_ref().unwrap().overall).collect();
    assert_eq!(
        scores,
        vec![1.0, 0.5, 0.0],
        "a reordered response must not shift scores onto the wrong cases"
    );
}

/// A dropped entry must fail exactly one case. Zipping by position would instead slide case 2's
/// verdict onto case 1 and case 3's onto case 2 — three wrong scores, all of them plausible.
#[test]
fn a_dropped_verdict_fails_only_its_own_case() {
    let r = simple_rubric();
    let cs = cases(&["a", "b", "c"]);
    let body = response(vec![verdict(0, 1.0), verdict(2, 0.0)]); // case-1 missing
    let out = run(&r, &cs, &[&body], 1);

    assert_eq!(out[0].as_ref().unwrap().overall, 1.0);
    assert!(out[1].is_err(), "the omitted case must fail, not inherit");
    assert_eq!(out[2].as_ref().unwrap().overall, 0.0);
}

#[test]
fn duplicate_and_unknown_ids_cannot_claim_a_case() {
    let r = simple_rubric();
    let cs = cases(&["a", "b"]);
    let body = response(vec![
        verdict(0, 1.0),
        verdict(0, 0.0), // duplicate: the first attribution wins, the second is dropped
        verdict(99, 1.0), // invented id: belongs to no case
    ]);
    let out = run(&r, &cs, &[&body], 1);

    assert_eq!(out[0].as_ref().unwrap().overall, 1.0);
    assert!(out[1].is_err(), "case-1 was never answered");
}

/// An entry that will not say which case it answers is unattributable and must be discarded rather
/// than assigned to whichever case happens to be next.
#[test]
fn an_entry_without_an_id_is_discarded() {
    let r = simple_rubric();
    let cs = cases(&["a"]);
    let body =
        json!({ "verdicts": [{ "correctness": { "score": 1.0, "reasoning": "r" } }] }).to_string();
    let gen = FakeBatch {
        responses: vec![body],
    };
    // Every sample fails to attribute anything, so the batch call reports a parse failure.
    let err = batch_with(&gen, &r, &cs, "fake", 1, 1);
    assert!(err.is_err() || err.unwrap()[0].is_err());
}

/// Cost is one call's, split across the cases that shared it; latency is the batch's wall clock for
/// every case, because that is what actually elapsed.
#[test]
fn cost_is_amortized_and_latency_is_shared() {
    let r = simple_rubric();
    let cs = cases(&["a", "b", "c", "d"]);
    let body = response((0..4).map(|i| verdict(i, 1.0)).collect());
    let out = run(&r, &cs, &[&body], 1);

    let first = out[0].as_ref().unwrap();
    assert_eq!(first.batch_size, Some(4));
    assert!(
        (first.cost_usd.unwrap() - 0.025).abs() < 1e-9,
        "0.10 across 4 cases = 0.025, got {:?}",
        first.cost_usd
    );
    assert_eq!(
        first.latency_ms,
        Some(400),
        "the batch's wall clock is every case's latency; dividing it would invent a number"
    );
}

/// A verdict judged alone carries no batch stamp, so a consumer can always tell an amortized figure
/// from a measured one.
#[test]
fn an_unbatched_verdict_carries_no_batch_stamp() {
    struct One;
    impl Generator for One {
        fn generate(&self, _i: usize, _p: &str) -> Result<GenOutcome> {
            Ok(gen_outcome(
                json!({ "correctness": { "score": 1.0, "reasoning": "r" } }).to_string(),
            ))
        }
    }
    let p = crate::prompts::Prompt::plain("x");
    let out = crate::judge::judge_with(&One, &simple_rubric(), &p, "fake", 1, 1, &[]).unwrap();
    assert_eq!(
        out.batch_size, None,
        "only a batched caller stamps a size; a lone verdict must stay unmarked"
    );
}

/// Self-consistency still works batched: k samples of the whole batch, aggregated per case. Sample 2
/// disagrees about case 0, and only case 0's agreement should move.
#[test]
fn samples_aggregate_per_case() {
    let r = simple_rubric();
    let cs = cases(&["a", "b"]);
    let s1 = response(vec![verdict(0, 1.0), verdict(1, 1.0)]);
    let s2 = response(vec![verdict(0, 0.0), verdict(1, 1.0)]);
    let out = run(&r, &cs, &[&s1, &s2], 2);

    let a = out[0].as_ref().unwrap();
    let b = out[1].as_ref().unwrap();
    assert_eq!(a.overall, 0.5, "0.0 and 1.0 average to 0.5");
    assert_eq!(a.samples_parsed, 2);
    assert_eq!(b.overall, 1.0);
    assert!(
        a.agreement < b.agreement,
        "the case the judge flip-flopped on must show lower agreement ({} vs {})",
        a.agreement,
        b.agreement
    );
}

/// Rotation is what turns a position effect into cross-sample disagreement instead of a silent bias.
#[test]
fn case_order_rotates_between_samples() {
    assert_eq!(rotated(3, 0), vec![0, 1, 2]);
    assert_eq!(rotated(3, 1), vec![1, 2, 0]);
    assert_eq!(rotated(3, 2), vec![2, 0, 1]);
    assert_eq!(
        rotated(1, 5),
        vec![0],
        "a single case has nowhere to rotate"
    );
}

/// An all-deterministic rubric is judged locally: no batch call, no tokens — the same promise the
/// single-case path makes.
#[test]
fn an_all_deterministic_rubric_makes_no_call() {
    let r = rubric(json!({
        "name": "det",
        "threshold": 0.5,
        "dimensions": [{
            "key": "exact", "description": "matches", "weight": 1.0,
            "kind": "contains", "check": { "expect": "yes" }
        }]
    }));
    let cs = cases(&["yes it is", "no"]);
    struct Exploding;
    impl Generator for Exploding {
        fn generate(&self, _i: usize, _p: &str) -> Result<GenOutcome> {
            panic!("an all-deterministic rubric must not call the provider");
        }
    }
    let out = batch_with(&Exploding, &r, &cs, "fake", 3, 1).unwrap();
    assert_eq!(out[0].as_ref().unwrap().overall, 1.0);
    assert_eq!(out[1].as_ref().unwrap().overall, 0.0);
    assert_eq!(out[0].as_ref().unwrap().samples, 0);
}
