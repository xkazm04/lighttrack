//! Judge tests for deterministic (non-LLM) rubric dimensions: they score locally at zero cost,
//! gate through the same floors, stay out of the judge prompt, and never inflate agreement.

use super::*;

/// Judge a case end-to-end through the deterministic scorers + the fake LLM generator, exactly as
/// [`run_rubric_judge`] does (minus the provider call).
fn judge_case(
    r: &Rubric,
    expected: Option<&str>,
    output: &str,
    llm_outputs: &[&str],
    samples: u32,
    jobs: usize,
) -> Result<RubricOutcome> {
    let det = crate::scorers::evaluate_all(r, expected, output)?;
    let prompt = build_rubric_prompt(r, "q", expected, output);
    judge_with(&FakeGen::new(llm_outputs), r, &prompt, "fake-model", samples, jobs, &det)
}

/// A generator that must never be called — proves an all-deterministic rubric spends nothing.
struct NeverGen;

impl Generator for NeverGen {
    fn generate(&self, _index: usize, _prompt: &str) -> Result<GenOutcome> {
        Err(EngineError::Other("the judge model must not be called".into()))
    }
}

fn mixed_rubric() -> Rubric {
    rubric(serde_json::json!({
        "name": "mixed",
        "threshold": 0.7,
        "dimensions": [
            { "key": "style", "description": "reads well", "weight": 1.0 },
            { "key": "answer", "description": "the exact answer", "weight": 3.0,
              "kind": "exact", "floor": 1.0 }
        ]
    }))
}

#[test]
fn deterministic_dimension_scores_locally_inside_the_same_math() {
    let r = mixed_rubric();
    // The candidate is right, and the LLM likes the style: weighted (1.0*3 + 0.8*1)/4 = 0.95.
    let out = judge_case(&r, Some("Paris"), "Paris", &[r#"{"style":{"score":0.8}}"#], 1, 1).unwrap();
    assert!((dim_score(&out, "answer") - 1.0).abs() < 1e-9);
    assert!((out.overall - 0.95).abs() < 1e-9, "overall {}", out.overall);
    assert!(out.pass);
    // Deterministic dimensions cost nothing: the only tokens/cost here are the LLM dimension's.
    let answer = out.dimensions.iter().find(|d| d.key == "answer").unwrap();
    assert_eq!(answer.reasonings.len(), 1, "a mechanical verdict records exactly one reason");
    assert!(answer.reasoning().contains("expected `Paris`"), "{}", answer.reasoning());
}

#[test]
fn deterministic_dimension_gates_the_case_through_its_floor() {
    let r = mixed_rubric();
    // Wrong answer (0.0, below its 1.0 floor) but a glowing LLM style score.
    let out = judge_case(&r, Some("Paris"), "Berlin", &[r#"{"style":{"score":1.0}}"#], 1, 1).unwrap();
    let answer = out.dimensions.iter().find(|d| d.key == "answer").unwrap();
    assert_eq!(answer.score, 0.0);
    assert!(answer.floor_hit, "the mechanical dimension must gate exactly like an LLM one");
    assert!(!out.pass);
    assert!(answer.reasoning().ends_with("→ fail"), "{}", answer.reasoning());
}

#[test]
fn a_deterministic_dimension_is_never_narrated_to_the_judge() {
    let r = mixed_rubric();
    let prompt = build_rubric_prompt(&r, "q", Some("Paris"), "Paris");
    assert!(prompt.text.contains("- style (weight 1)"));
    assert!(!prompt.text.contains("answer"), "a locally-checked dimension must not be asked for");
    let schema = build_rubric_schema(&r);
    assert!(schema["properties"].get("style").is_some());
    assert!(schema["properties"].get("answer").is_none(), "schema must not request it either");
    // …and a judge response that omits it is still perfectly parseable.
    assert!(judge_case(&r, Some("Paris"), "Paris", &[r#"{"style":{"score":0.5}}"#], 1, 1).is_ok());
}

#[test]
fn deterministic_dimensions_do_not_inflate_agreement() {
    let r = mixed_rubric();
    // The LLM dimension swings 1.0 → 0.0 across samples; the exact check is fixed at 1.0. Agreement
    // must report the judge's disagreement, not be dragged toward 1.0 by the reproducible dimension.
    let out = judge_case(
        &r,
        Some("Paris"),
        "Paris",
        &[r#"{"style":{"score":1.0}}"#, r#"{"style":{"score":0.0}}"#],
        2,
        1,
    )
    .unwrap();
    assert_eq!(out.agreement, 0.0, "agreement covers the LLM dimensions only");
    assert_eq!(out.samples_parsed, 2);
}

#[test]
fn an_all_deterministic_rubric_makes_no_provider_call() {
    let r = rubric(serde_json::json!({
        "name": "mech",
        "threshold": 1.0,
        "dimensions": [
            { "key": "answer", "description": "", "weight": 1.0, "kind": "exact" },
            { "key": "shape", "description": "", "weight": 1.0, "kind": "json_valid" }
        ]
    }));
    let output = r#"{"city":"Paris"}"#;
    let det = crate::scorers::evaluate_all(&r, Some(output), output).unwrap();
    let prompt = build_rubric_prompt(&r, "q", Some(output), output);
    // NeverGen errors if called — reaching an Ok outcome proves zero provider calls happened.
    let out = judge_with(&NeverGen, &r, &prompt, "fake-model", 5, 4, &det).unwrap();
    assert_eq!(out.overall, 1.0);
    assert!(out.pass);
    assert_eq!(out.samples, 0, "nothing was sampled");
    assert_eq!(out.samples_parsed, 0);
    assert_eq!(out.cost_usd, None, "zero cost");
    assert_eq!(out.tokens, Some(0), "zero tokens");
    assert_eq!(out.model, "deterministic", "no model scored this case");
    assert_eq!(out.agreement, 1.0);
    assert_eq!(out.determinism, Determinism::Exact);
}

#[test]
fn a_misconfigured_deterministic_dimension_is_a_loud_error() {
    let r = rubric(serde_json::json!({
        "name": "bad",
        "threshold": 0.5,
        "dimensions": [ { "key": "answer", "description": "", "weight": 1.0, "kind": "regex" } ]
    }));
    // No pattern to match against: an operator bug, never a candidate that "scored 0".
    match judge_case(&r, None, "anything", &[], 1, 1) {
        Err(EngineError::Other(m)) => assert!(m.contains("check.pattern"), "{m}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

#[test]
fn an_all_llm_rubric_is_unaffected_by_the_new_fields() {
    // The legacy rubric shape (no `kind`, no `check`) must judge exactly as it did before: same
    // dimensions asked of the model, same means, same overall, same gating.
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.7,
        "dimensions": [
            { "key": "a", "description": "", "weight": 3.0 },
            { "key": "b", "description": "", "weight": 1.0, "floor": 0.5 }
        ]
    }));
    assert!(crate::scorers::evaluate_all(&r, Some("ref"), "out").unwrap().is_empty());
    let out = judge_case(
        &r,
        Some("ref"),
        "out",
        &[r#"{"a":{"score":0.8,"reasoning":"ok"},"b":{"score":0.4}}"#],
        1,
        1,
    )
    .unwrap();
    assert!((out.overall - 0.7).abs() < 1e-9, "overall {}", out.overall);
    assert_eq!(out.samples, 1);
    assert_eq!(out.model, "fake", "the judge model still scored it");
    assert!(!out.pass, "b is below its floor");
    assert_eq!(out.dimensions[0].reasonings, vec!["ok"]);
}
