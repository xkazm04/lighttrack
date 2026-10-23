//! Unit tests for the deterministic dimension kinds: what they score, why, and which failures are
//! the operator's (hard error) rather than the candidate's (0.0 with a reason).

use super::*;
use serde_json::json;

fn dim(v: serde_json::Value) -> RubricDimension {
    serde_json::from_value(v).expect("dimension literal")
}

/// Score one dimension, surfacing the `Result` so misconfiguration can be asserted on.
fn score(v: serde_json::Value, expected: Option<&str>, output: &str) -> Result<(f64, String)> {
    // Every kind exercised here is a local text check, which always yields a score; only `exec` can
    // void, and it has its own tests beside the sandbox it needs.
    let (s, reasoning) = evaluate(&dim(v), expected, output, None)?;
    Ok((s.expect("a text kind always scores"), reasoning))
}

fn ok(v: serde_json::Value, expected: Option<&str>, output: &str) -> (f64, String) {
    score(v, expected, output).expect("deterministic check")
}

#[test]
fn exact_defaults_to_the_case_reference_and_ignores_case() {
    let d = json!({ "key": "answer", "description": "", "kind": "exact" });
    let (s, why) = ok(d.clone(), Some("Paris"), "  paris ");
    assert_eq!(s, 1.0, "trim + case-insensitive by default: {why}");
    assert!(
        why.contains("expected `Paris`") && why.ends_with("→ pass"),
        "{why}"
    );
    assert_eq!(ok(d, Some("Paris"), "Berlin").0, 0.0);
}

#[test]
fn exact_can_demand_case_sensitivity_and_its_own_target() {
    let d = json!({
        "key": "answer", "description": "", "kind": "exact",
        "check": { "expect": "OK", "case_sensitive": true }
    });
    // The dimension's own `expect` wins over the case's reference.
    assert_eq!(ok(d.clone(), Some("ignored"), "OK").0, 1.0);
    assert_eq!(ok(d, Some("ignored"), "ok").0, 0.0);
}

#[test]
fn exact_without_any_target_is_an_operator_error() {
    let err = score(
        json!({ "key": "a", "description": "", "kind": "exact" }),
        None,
        "x",
    )
    .expect_err("no target must not be a silent 0.0");
    match err {
        EngineError::Other(m) => assert!(m.contains("has no target") && m.contains("'a'"), "{m}"),
        other => panic!("expected Other, got {other:?}"),
    }
}

#[test]
fn contains_looks_for_a_substring() {
    let d = json!({
        "key": "cites", "description": "", "kind": "contains",
        "check": { "expect": "doi:10.1000" }
    });
    assert_eq!(
        ok(d.clone(), None, "see doi:10.1000/xyz for details").0,
        1.0
    );
    let (s, why) = ok(d, None, "no citation here");
    assert_eq!(s, 0.0);
    assert!(why.contains("looked for `doi:10.1000`"), "{why}");
}

#[test]
fn regex_matches_unanchored_and_case_insensitively_by_default() {
    let d = json!({
        "key": "format", "description": "", "kind": "regex",
        "check": { "pattern": "^ERROR: [a-z ]+$" }
    });
    assert_eq!(ok(d.clone(), None, "error: disk full").0, 1.0);
    assert_eq!(ok(d, None, "all good").0, 0.0);
}

#[test]
fn regex_misconfiguration_is_an_operator_error() {
    assert!(matches!(
        score(
            json!({ "key": "f", "description": "", "kind": "regex" }),
            None,
            "x"
        ),
        Err(EngineError::Other(_))
    ));
    assert!(matches!(
        score(
            json!({ "key": "f", "description": "", "kind": "regex", "check": { "pattern": "(unclosed" } }),
            None,
            "x"
        ),
        Err(EngineError::Other(_))
    ));
}

#[test]
fn numeric_respects_tolerance_and_explains_itself() {
    let d = json!({
        "key": "total", "description": "", "kind": "numeric",
        "check": { "expect": "42.0", "tolerance": 0.1 }
    });
    let (s, why) = ok(d.clone(), None, "41.6");
    assert_eq!(s, 0.0);
    assert!(
        why.contains("expected `42`")
            && why.contains("got `41.6`")
            && why.contains("tolerance 0.1"),
        "a mechanical verdict must be auditable: {why}"
    );
    assert_eq!(
        ok(d.clone(), None, "The total is 41.95 dollars.").0,
        1.0,
        "within tolerance"
    );
    let (s, why) = ok(d, None, "no digits at all");
    assert_eq!(s, 0.0);
    assert!(why.contains("no number"), "{why}");
}

#[test]
fn numeric_integer_targets_compare_exactly_beyond_f64_precision() {
    let d = json!({ "key": "total", "description": "", "kind": "numeric" });
    let expected = "207243426028869626379158320647863778";

    assert_eq!(ok(d.clone(), Some(expected), expected).0, 1.0);
    assert_eq!(
        ok(d, Some(expected), "207243426028869626379158320647863779").0,
        0.0,
        "distinct integer answers must not collapse to the same floating-point value"
    );
}

#[test]
fn numeric_integer_targets_do_not_use_decimal_tolerance() {
    let d = json!({
        "key": "total", "description": "", "kind": "numeric",
        "check": { "expect": "42", "tolerance": 0.1 }
    });

    assert_eq!(ok(d.clone(), None, "42.0").0, 1.0);
    assert_eq!(ok(d, None, "41.95").0, 0.0);
}

#[test]
fn numeric_target_must_be_a_number() {
    assert!(matches!(
        score(
            json!({ "key": "n", "description": "", "kind": "numeric" }),
            Some("about ten"),
            "10"
        ),
        Err(EngineError::Other(_))
    ));
}

#[test]
fn json_valid_checks_parseability() {
    let d = json!({ "key": "shape", "description": "", "kind": "json_valid" });
    let (s, why) = ok(d.clone(), None, r#"{"a":1}"#);
    assert_eq!(s, 1.0);
    assert!(why.contains("parses as JSON"), "{why}");
    assert_eq!(ok(d, None, "sorry, I can't do that").0, 0.0);
}

#[test]
fn a_json_pointer_narrows_every_kind() {
    let d = json!({
        "key": "city", "description": "", "kind": "exact",
        "check": { "expect": "Paris", "path": "/data/city" }
    });
    assert_eq!(ok(d.clone(), None, r#"{"data":{"city":"Paris"}}"#).0, 1.0);
    // A path that cannot be read is the candidate's failure, not the operator's.
    let (s, why) = ok(d.clone(), None, r#"{"data":{}}"#);
    assert_eq!(s, 0.0);
    assert!(why.contains("no value at path `/data/city`"), "{why}");
    let (s, why) = ok(d, None, "not json");
    assert_eq!(s, 0.0);
    assert!(why.contains("is not JSON"), "{why}");
}

#[test]
fn a_pointed_number_is_compared_numerically() {
    let d = json!({
        "key": "total", "description": "", "kind": "numeric",
        "check": { "expect": "42.0", "path": "/total", "tolerance": 0.5 }
    });
    assert_eq!(ok(d, None, r#"{"total": 41.7}"#).0, 1.0);
}

#[test]
fn evaluate_all_covers_only_deterministic_dimensions_in_rubric_order() {
    let r: Rubric = serde_json::from_value(json!({
        "name": "mixed", "threshold": 0.7,
        "dimensions": [
            { "key": "style", "description": "reads well", "weight": 1.0 },
            { "key": "answer", "description": "", "weight": 3.0, "kind": "exact" },
            { "key": "json", "description": "", "weight": 1.0, "kind": "json_valid" }
        ]
    }))
    .expect("rubric");
    assert!(has_llm_dims(&r));
    let det = evaluate_all(&r, Some("{\"a\":1}"), "{\"a\":1}", None).expect("evaluate");
    assert_eq!(
        det.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(),
        ["answer", "json"]
    );
    assert!(det.iter().all(|d| d.score == Some(1.0)));
}

#[test]
fn an_all_deterministic_rubric_has_no_llm_dimensions() {
    let r: Rubric = serde_json::from_value(json!({
        "name": "mech", "threshold": 1.0,
        "dimensions": [ { "key": "answer", "description": "", "kind": "exact" } ]
    }))
    .expect("rubric");
    assert!(!has_llm_dims(&r));
}

#[test]
fn long_values_are_snipped_in_the_reasoning() {
    let long = "x".repeat(500);
    let (_, why) = ok(
        json!({ "key": "a", "description": "", "kind": "exact", "check": { "expect": "y" } }),
        None,
        &long,
    );
    assert!(
        why.contains('…'),
        "an oversized value must be marked as truncated: {why}"
    );
    assert!(why.chars().count() < 300, "reasoning stayed bounded");
}
