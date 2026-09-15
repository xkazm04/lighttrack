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
    let det = crate::scorers::evaluate_all(r, expected, output, None)?;
    let prompt = build_rubric_prompt(r, "q", expected, output);
    judge_with(
        &FakeGen::new(llm_outputs),
        r,
        &prompt,
        "fake-model",
        samples,
        jobs,
        &det,
    )
}

/// A generator that must never be called — proves an all-deterministic rubric spends nothing.
struct NeverGen;

impl Generator for NeverGen {
    fn generate(&self, _index: usize, _prompt: &str) -> Result<GenOutcome> {
        Err(EngineError::Other(
            "the judge model must not be called".into(),
        ))
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
    let out = judge_case(
        &r,
        Some("Paris"),
        "Paris",
        &[r#"{"style":{"score":0.8}}"#],
        1,
        1,
    )
    .unwrap();
    assert!((dim_score(&out, "answer") - 1.0).abs() < 1e-9);
    assert!((out.overall - 0.95).abs() < 1e-9, "overall {}", out.overall);
    assert!(out.pass);
    // Deterministic dimensions cost nothing: the only tokens/cost here are the LLM dimension's.
    let answer = out.dimensions.iter().find(|d| d.key == "answer").unwrap();
    assert_eq!(
        answer.reasonings.len(),
        1,
        "a mechanical verdict records exactly one reason"
    );
    assert!(
        answer.reasoning().contains("expected `Paris`"),
        "{}",
        answer.reasoning()
    );
}

#[test]
fn deterministic_dimension_gates_the_case_through_its_floor() {
    let r = mixed_rubric();
    // Wrong answer (0.0, below its 1.0 floor) but a glowing LLM style score.
    let out = judge_case(
        &r,
        Some("Paris"),
        "Berlin",
        &[r#"{"style":{"score":1.0}}"#],
        1,
        1,
    )
    .unwrap();
    let answer = out.dimensions.iter().find(|d| d.key == "answer").unwrap();
    assert_eq!(answer.score, 0.0);
    assert!(
        answer.floor_hit,
        "the mechanical dimension must gate exactly like an LLM one"
    );
    assert!(!out.pass);
    assert!(
        answer.reasoning().ends_with("→ fail"),
        "{}",
        answer.reasoning()
    );
}

#[test]
fn a_deterministic_dimension_is_never_narrated_to_the_judge() {
    let r = mixed_rubric();
    let prompt = build_rubric_prompt(&r, "q", Some("Paris"), "Paris");
    assert!(prompt.text.contains("- style (weight 1)"));
    assert!(
        !prompt.text.contains("answer"),
        "a locally-checked dimension must not be asked for"
    );
    let schema = build_rubric_schema(&r);
    assert!(schema["properties"].get("style").is_some());
    assert!(
        schema["properties"].get("answer").is_none(),
        "schema must not request it either"
    );
    // …and a judge response that omits it is still perfectly parseable.
    assert!(judge_case(
        &r,
        Some("Paris"),
        "Paris",
        &[r#"{"style":{"score":0.5}}"#],
        1,
        1
    )
    .is_ok());
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
    assert_eq!(
        out.agreement, 0.0,
        "agreement covers the LLM dimensions only"
    );
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
    let det = crate::scorers::evaluate_all(&r, Some(output), output, None).unwrap();
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
    assert!(crate::scorers::evaluate_all(&r, Some("ref"), "out", None)
        .unwrap()
        .is_empty());
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

// ---------------------------------------------------------------------------------------------
// Voiding: the `exec` kind's third outcome, seen from the aggregation that has to honour it.
// These build the `DetScore` directly rather than through a sandbox — the transport has its own
// tests beside it, and what is under test here is the arithmetic.
// ---------------------------------------------------------------------------------------------

fn voided(key: &str) -> crate::scorers::DetScore {
    crate::scorers::DetScore {
        key: key.to_string(),
        score: None,
        reasoning: "exec: unavailable (contree exited 1: capacity) → voided, not scored".into(),
    }
}

fn exec_plus_llm() -> Rubric {
    rubric(serde_json::json!({
        "name": "codegen",
        "threshold": 0.7,
        "dimensions": [
            { "key": "passes", "description": "", "weight": 3.0, "floor": 1.0,
              "kind": "exec",
              "check": { "image": "tag:x:v1", "cmd": "pytest -q", "write": "/work/s.py" } },
            { "key": "idiomatic", "description": "", "weight": 1.0 }
        ]
    }))
}

/// The heart of the rule: an outage must not become a heavily-weighted zero that drags the case
/// down. The voided dimension leaves the denominator too, so the dimensions that *were* measured
/// keep their relative weights.
#[test]
fn a_voided_dimension_leaves_the_numerator_and_the_denominator() {
    let r = exec_plus_llm();
    let prompt = build_rubric_prompt(&r, "q", None, "out");
    let out = judge_with(
        &FakeGen::new(&[r#"{"idiomatic":{"score":0.8,"reasoning":"fine"}}"#]),
        &r,
        &prompt,
        "fake-model",
        1,
        1,
        &[voided("passes")],
    )
    .expect("a voided dimension is not a failed case");

    assert!(
        (out.overall - 0.8).abs() < 1e-9,
        "overall is the measured dimension alone, not 0.8*1/4: {}",
        out.overall
    );
    assert!(out.pass, "0.8 clears the 0.7 threshold");
    let passes = &out.dimensions[0];
    assert!(passes.voided);
    assert!(
        !passes.floor_hit,
        "a measurement that did not happen cannot breach a floor"
    );
    assert!(
        passes.reasonings[0].contains("unavailable"),
        "the audit trail says why"
    );
}

/// The same rubric with the sandbox *working* and the candidate genuinely failing must still fail —
/// otherwise voiding would be a way to launder a bad candidate into a pass.
#[test]
fn a_real_exec_failure_still_fails_the_case() {
    let r = exec_plus_llm();
    let prompt = build_rubric_prompt(&r, "q", None, "out");
    let failed = crate::scorers::DetScore {
        key: "passes".into(),
        score: Some(0.0),
        reasoning: "exec: exit 1 → fail".into(),
    };
    let out = judge_with(
        &FakeGen::new(&[r#"{"idiomatic":{"score":1.0}}"#]),
        &r,
        &prompt,
        "fake-model",
        1,
        1,
        &[failed],
    )
    .expect("outcome");
    assert!(!out.dimensions[0].voided);
    assert!(out.dimensions[0].floor_hit, "0.0 is below the 1.0 floor");
    assert!(!out.pass);
}

/// Nothing was measured, so there is nothing to report. A confident 0.0/fail here would be the
/// exact lie the kind exists to avoid.
#[test]
fn a_case_whose_every_dimension_voided_has_no_verdict() {
    let r = rubric(serde_json::json!({
        "name": "exec-only",
        "threshold": 0.5,
        "dimensions": [
            { "key": "passes", "description": "", "weight": 1.0, "kind": "exec",
              "check": { "image": "tag:x:v1", "cmd": "true", "write": "/w/s.py" } }
        ]
    }));
    let prompt = build_rubric_prompt(&r, "q", None, "out");
    match judge_with(
        &NeverGen,
        &r,
        &prompt,
        "fake-model",
        1,
        1,
        &[voided("passes")],
    ) {
        Err(EngineError::Other(m)) => {
            assert!(m.contains("no"), "says there is no verdict: {m}");
            assert!(m.contains("voided"), "{m}");
        }
        other => panic!("expected no verdict, got {other:?}"),
    }
}

/// A rubric that needs a sandbox and was given none is refused by name — never scored as if the
/// dimension were absent.
#[test]
fn an_exec_rubric_without_a_sandbox_is_refused_not_skipped() {
    let r = exec_plus_llm();
    assert!(crate::scorers::needs_sandbox(&r));
    match crate::scorers::evaluate_all(&r, None, "out", None) {
        Err(EngineError::Other(m)) => {
            assert!(m.contains("passes") && m.contains("no sandbox"), "{m}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}
