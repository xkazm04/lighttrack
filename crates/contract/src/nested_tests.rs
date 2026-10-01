use serde_json::Value;

use super::*;

fn item_required(raw: &str) -> Vec<String> {
    let s: Value = serde_json::from_str(raw).expect("valid JSON");
    s["items"]["required"]
        .as_array()
        .expect("required array")
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect()
}

#[test]
fn rubric_dimensions_require_key_and_description() {
    assert_eq!(item_required(RUBRIC_DIMENSIONS), ["key", "description"]);
    let s: Value = serde_json::from_str(RUBRIC_DIMENSIONS).expect("valid JSON");
    let props = &s["items"]["properties"];
    for f in ["key", "description", "weight", "anchors", "floor"] {
        assert!(props.get(f).is_some(), "missing {f}");
    }
    assert_eq!(props["anchors"]["items"]["type"], "string");
}

/// The same gap on the rubric. `kind` is how a dimension stops being a judge call, and
/// `check` is how that kind is configured. Neither is in the schema an agent receives, so
/// every rubric an agent writes is all-`llm` even though the API accepts eight kinds.
#[test]
fn rubric_dimensions_name_kind_and_check() {
    let s: Value = serde_json::from_str(RUBRIC_DIMENSIONS).expect("valid JSON");
    let props = &s["items"]["properties"];
    let kind = &props["kind"];
    assert!(
        !kind.is_null(),
        "an agent is never told a dimension can be anything but the judge"
    );
    assert_eq!(
        kind["enum"],
        serde_json::json!([
            "llm",
            "exact",
            "regex",
            "numeric",
            "json_valid",
            "contains",
            "exec",
            "grounding"
        ]),
        "every kind the engine parses, default first"
    );
    let check = props.get("check").expect("check is offered");
    for f in [
        "expect",
        "pattern",
        "tolerance",
        "path",
        "case_sensitive",
        "trim",
        "image",
        "cmd",
        "write",
        "timeout_secs",
        "env",
    ] {
        assert!(check["properties"].get(f).is_some(), "check.{f}");
    }
    assert_eq!(check["properties"]["env"]["type"], "object");
    let required = item_required(RUBRIC_DIMENSIONS);
    assert!(
        !required.contains(&"kind".to_string()),
        "kind defaults to llm"
    );
    assert!(
        !required.contains(&"check".to_string()),
        "an llm dimension omits check"
    );
}

#[test]
fn benchmark_dataset_requires_only_input() {
    assert_eq!(item_required(BENCHMARK_DATASET), ["input"]);
    let s: Value = serde_json::from_str(BENCHMARK_DATASET).expect("valid JSON");
    for f in ["input", "expected", "output"] {
        assert!(s["items"]["properties"].get(f).is_some(), "missing {f}");
    }
}

/// **An axis an agent cannot see is an axis nobody uses.** A live benchmark on 2026-09-07 graded
/// 8 of its 18 cases `expert` - a rung that does not exist - because nothing in the contract said
/// what the ladder was. Those 8 were silently stored ungraded (that silence is now a 400), and
/// the per-tier report covered a corpus whose grading had been discarded. The enum has to be IN
/// the schema, not merely enforced behind it.
#[test]
fn benchmark_dataset_names_the_difficulty_ladder() {
    let s: Value = serde_json::from_str(BENCHMARK_DATASET).expect("valid JSON");
    let d = &s["items"]["properties"]["difficulty"];
    assert!(!d.is_null(), "an agent is never told the field exists");
    assert_eq!(
        d["enum"],
        serde_json::json!(["easy", "medium", "hard"]),
        "the closed ladder is spelled out, in ascending order"
    );
    assert!(
        !item_required(BENCHMARK_DATASET).contains(&"difficulty".to_string()),
        "ungraded stays legal - grading is opt-in"
    );
}

/// The same gap on the other wave-1 axis: `effort` was shipped, works, and appeared nowhere an
/// agent could read it, so the matrix an agent builds could never vary the knob the benchmark
/// exists to compare.
#[test]
fn benchmark_targets_name_the_effort_axis() {
    let s: Value = serde_json::from_str(BENCHMARK_TARGETS).expect("valid JSON");
    let e = &s["items"]["properties"]["effort"];
    assert!(!e.is_null(), "an agent is never told the axis exists");
    assert_eq!(
        e["enum"],
        serde_json::json!(["low", "medium", "high", "xhigh", "max"]),
        "every level the engine parses, in ascending order"
    );
    assert!(
        !item_required(BENCHMARK_TARGETS).contains(&"effort".to_string()),
        "an absent effort means the provider default, which is a real and different choice"
    );
}

/// The third axis an agent cannot use if it cannot see it: a quality bar with no cost or latency
/// bar beside it is how a benchmark certifies a configuration nobody could afford to run.
#[test]
fn benchmark_targets_name_the_per_case_limits() {
    let s: Value = serde_json::from_str(BENCHMARK_TARGETS).expect("valid JSON");
    let l = &s["items"]["properties"]["limits"];
    assert!(!l.is_null(), "an agent is never told limits exist");
    for f in ["max_cost_usd", "max_latency_ms"] {
        assert!(l["properties"].get(f).is_some(), "limits.{f}");
    }
    assert!(
        !item_required(BENCHMARK_TARGETS).contains(&"limits".to_string()),
        "limits stay opt-in"
    );
}

#[test]
fn benchmark_targets_use_system_prompt_not_prompt() {
    assert_eq!(item_required(BENCHMARK_TARGETS), ["provider", "model"]);
    let s: Value = serde_json::from_str(BENCHMARK_TARGETS).expect("valid JSON");
    let props = &s["items"]["properties"];
    assert!(props.get("system_prompt").is_some());
    assert!(
        props.get("prompt").is_none(),
        "the real field is system_prompt"
    );
    assert!(props.get("label").is_some());
}

/// An agent that cannot see `prompt_ref` writes benchmarks whose runs can never satisfy the
/// promotion gate — so the schema has to carry it, spelled exactly as the API deserializes it.
#[test]
fn benchmark_targets_expose_the_resolvable_and_http_shapes() {
    let s: Value = serde_json::from_str(BENCHMARK_TARGETS).expect("valid JSON");
    let props = &s["items"]["properties"];
    let pr = props.get("prompt_ref").expect("prompt_ref is offered");
    assert_eq!(pr["required"], serde_json::json!(["name"]));
    for f in ["name", "version", "label"] {
        assert!(pr["properties"].get(f).is_some(), "prompt_ref.{f}");
    }
    let kind = props.get("kind").expect("kind is offered");
    assert_eq!(
        kind["properties"]["type"]["enum"],
        serde_json::json!(["model", "http"])
    );
    assert!(kind["properties"].get("url").is_some());
}
