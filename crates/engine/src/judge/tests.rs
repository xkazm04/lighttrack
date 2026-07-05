//! Unit tests for the judge: JSON extraction, spec parsing, and the rubric scoring/gating math
//! driven through the [`Generator`] seam with a deterministic fake (no live API calls).

use super::*;

#[test]
fn verdict_from_judge_text() {
    let text = "Here is my verdict:\n```json\n{\"score\":0.2,\"max\":1.0,\"pass\":false,\"reasoning\":\"wrong\"}\n```";
    let json = extract_json_object(text).unwrap();
    let v: JudgeVerdict = serde_json::from_str(&json).unwrap();
    assert_eq!(v.score, 0.2);
    assert!(!v.pass);
}

#[test]
fn rubric_json_from_text() {
    let v = extract_json_value("noise {\"correctness\":{\"score\":0.9,\"reasoning\":\"ok\"}} tail");
    assert_eq!(v["correctness"]["score"], 0.9);
    assert!(extract_json_value("no json").is_null());
}

#[test]
fn extracts_object() {
    assert_eq!(extract_json_object("noise {\"a\":1} tail"), Some("{\"a\":1}".to_string()));
    assert_eq!(extract_json_object("no json here"), None);
}

#[test]
fn judge_spec_parsing() {
    assert_eq!(parse_judge_spec("haiku"), ("anthropic".into(), "haiku".into()));
    assert_eq!(
        parse_judge_spec("google/gemini-2.5-flash"),
        ("google".into(), "gemini-2.5-flash".into())
    );
}

/// Deterministic generator that replays canned JSON judge outputs, one per sample (cycling).
/// Lets us drive `judge_with` through the [`Generator`] seam with zero network/process calls.
struct FakeGen {
    outputs: Vec<String>,
    calls: usize,
}

impl FakeGen {
    fn new(outputs: &[&str]) -> Self {
        FakeGen { outputs: outputs.iter().map(|s| s.to_string()).collect(), calls: 0 }
    }
}

impl Generator for FakeGen {
    fn generate(&mut self, _prompt: &str) -> Result<GenOutcome> {
        let output = self.outputs[self.calls % self.outputs.len()].clone();
        self.calls += 1;
        Ok(GenOutcome {
            output,
            cost_usd: None,
            model: "fake".into(),
            latency_ms: Some(1),
            input_tokens: Some(0),
            output_tokens: Some(0),
        })
    }
}

/// Build a `Rubric` from a JSON literal (serde fills id/created_at/weights via defaults).
fn rubric(json: Value) -> Rubric {
    serde_json::from_value(json).unwrap()
}

/// Judge canned `outputs` against `r` over `samples`, surfacing the `Result` (no unwrap) so parse
/// failures can be asserted on.
fn try_judge(r: &Rubric, outputs: &[&str], samples: u32) -> Result<RubricOutcome> {
    let mut gen = FakeGen::new(outputs);
    judge_with(&mut gen, r, "prompt", "fake-model", samples)
}

/// Judge canned `outputs` against `r` over `samples`, via the fake generator.
fn judge(r: &Rubric, outputs: &[&str], samples: u32) -> RubricOutcome {
    try_judge(r, outputs, samples).unwrap()
}

fn dim_score(out: &RubricOutcome, key: &str) -> f64 {
    out.dimensions.iter().find(|d| d.key == key).unwrap().score
}

#[test]
fn subfloor_critical_dimension_forces_fail() {
    // safety is gated at 0.5 but weighted lightly; quality dominates the weighted mean.
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.7,
        "dimensions": [
            { "key": "safety", "description": "", "weight": 1.0, "floor": 0.5 },
            { "key": "quality", "description": "", "weight": 9.0 }
        ]
    }));
    // safety 0.2 (< floor), quality 1.0 => weighted 0.92 clears the 0.7 threshold...
    let out = judge(&r, &[r#"{"safety":{"score":0.2},"quality":{"score":1.0}}"#], 1);
    assert!(out.overall >= r.threshold, "overall {} should clear threshold", out.overall);
    // ...but the sub-floor critical dimension must still gate the case to a fail.
    assert!(!out.pass, "sub-floor critical dimension must force pass=false");
}

#[test]
fn weighted_overall_and_dimension_means() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [
            { "key": "a", "description": "", "weight": 3.0 },
            { "key": "b", "description": "", "weight": 1.0 }
        ]
    }));
    // a scores 0.8 then 0.6 (mean 0.7); b scores 0.4 both times.
    let out = judge(
        &r,
        &[
            r#"{"a":{"score":0.8},"b":{"score":0.4}}"#,
            r#"{"a":{"score":0.6},"b":{"score":0.4}}"#,
        ],
        2,
    );
    assert!((dim_score(&out, "a") - 0.7).abs() < 1e-9, "a mean {}", dim_score(&out, "a"));
    assert!((dim_score(&out, "b") - 0.4).abs() < 1e-9, "b mean {}", dim_score(&out, "b"));
    // weighted overall = (0.7*3 + 0.4*1) / 4 = 0.625
    assert!((out.overall - 0.625).abs() < 1e-9, "overall {}", out.overall);
}

#[test]
fn out_of_range_scores_clamp_to_unit_interval() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [
            { "key": "hi", "description": "", "weight": 1.0 },
            { "key": "lo", "description": "", "weight": 1.0 }
        ]
    }));
    let out = judge(&r, &[r#"{"hi":{"score":1.5},"lo":{"score":-0.3}}"#], 1);
    assert_eq!(dim_score(&out, "hi"), 1.0, "1.5 must clamp to 1.0");
    assert_eq!(dim_score(&out, "lo"), 0.0, "-0.3 must clamp to 0.0");
    assert_eq!(out.overall, 0.5);
}

#[test]
fn divergent_samples_lower_agreement() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [ { "key": "x", "description": "", "weight": 1.0 } ]
    }));
    // identical samples => full agreement.
    let agree = judge(&r, &[r#"{"x":{"score":0.8}}"#, r#"{"x":{"score":0.8}}"#], 2).agreement;
    assert_eq!(agree, 1.0);
    // overalls 1.0 vs 0.0 => agreement collapses, and is strictly below the identical case.
    let diverge = judge(&r, &[r#"{"x":{"score":1.0}}"#, r#"{"x":{"score":0.0}}"#], 2).agreement;
    assert!(diverge < agree, "divergent agreement {diverge} should be below {agree}");
    assert_eq!(diverge, 0.0);
}

#[test]
fn unparseable_output_errors_with_raw_text() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [ { "key": "x", "description": "", "weight": 1.0 } ]
    }));
    // No JSON object at all must NOT silently score 0.0 — it must error, carrying the raw output.
    let err = try_judge(&r, &["the model rambled but emitted no json"], 1).unwrap_err();
    match err {
        EngineError::Parse(m) => assert!(m.contains("rambled"), "raw output must be in error: {m}"),
        other => panic!("expected Parse error, got {other:?}"),
    }
}

#[test]
fn missing_or_nonnumeric_dimension_score_errors() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [ { "key": "x", "description": "", "weight": 1.0 } ]
    }));
    // Valid JSON object but the dimension's score is absent => error, not a phantom 0.0.
    assert!(matches!(
        try_judge(&r, &[r#"{"x":{"reasoning":"forgot the score"}}"#], 1),
        Err(EngineError::Parse(_))
    ));
    // Score present but non-numeric (a string) is likewise unparseable.
    assert!(matches!(
        try_judge(&r, &[r#"{"x":{"score":"high"}}"#], 1),
        Err(EngineError::Parse(_))
    ));
}

#[test]
fn partial_parse_failures_drop_phantom_zeros() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [ { "key": "x", "description": "", "weight": 1.0 } ]
    }));
    // One good sample (0.8) and one unparseable: the mean must be 0.8, NOT averaged with a phantom
    // 0.0 down to 0.4. The dropped sample is surfaced via parse_failures.
    let out = try_judge(&r, &[r#"{"x":{"score":0.8}}"#, "not json"], 2).unwrap();
    assert_eq!(out.parse_failures, 1, "the unparseable sample must be counted");
    assert_eq!(out.samples, 2, "samples reflects the requested count");
    assert!((dim_score(&out, "x") - 0.8).abs() < 1e-9, "mean {} must ignore the phantom zero", dim_score(&out, "x"));
    assert!((out.overall - 0.8).abs() < 1e-9, "overall {} must ignore the phantom zero", out.overall);
    // Only one sample actually scored, so there is no disagreement to measure.
    assert_eq!(out.agreement, 1.0);
}

#[test]
fn clean_samples_report_zero_parse_failures() {
    let r = rubric(serde_json::json!({
        "name": "t",
        "threshold": 0.0,
        "dimensions": [ { "key": "x", "description": "", "weight": 1.0 } ]
    }));
    let out = judge(&r, &[r#"{"x":{"score":0.5}}"#], 1);
    assert_eq!(out.parse_failures, 0);
}
