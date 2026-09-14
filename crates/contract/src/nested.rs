//! Full JSON schemas for the few write-tool arguments whose value is a nested object, or an array
//! of them.
//!
//! Everywhere else a parameter's type, enum and prose are enough. Here they are not: an agent that
//! is told `targets` is "an array" writes a benchmark whose runs can never satisfy the promotion
//! gate, because it cannot see that a target may carry a `prompt_ref` and that the literal field is
//! `system_prompt`, not `prompt`. These mirror exactly what the API deserialises — `RubricDimension`
//! (`crates/core/src/rubric.rs`) and `BenchmarkCase` / `BenchTarget` (`crates/core/src/score.rs`).
//!
//! Raw JSON text rather than a builder so the schema reads as the document an agent receives; the
//! contract's own test parses every one of them, so a typo is a failing build, not a silently
//! vaguer tool.

/// `create_rubric.dimensions` — weighted, anchored scoring dimensions. `key` + `description` are
/// required (neither has a serde default); `weight` / `anchors` / `floor` are optional.
pub(crate) const RUBRIC_DIMENSIONS: &str = r#"{
    "type": "array",
    "description": "weighted scoring dimensions the judge fills per case",
    "items": {
        "type": "object",
        "required": ["key", "description"],
        "properties": {
            "key": {"type":"string","description":"stable identifier used in the judge's JSON output (e.g. correctness)"},
            "description": {"type":"string","description":"what this dimension measures"},
            "weight": {"type":"number","description":"relative weight in the overall score (default 1.0)"},
            "anchors": {"type":"array","items":{"type":"string"},"description":"anchored level descriptions, e.g. [\"1.0 = fully correct\", \"0.5 = minor error\", \"0 = wrong\"]"},
            "floor": {"type":"number","description":"gating floor: if this dimension scores below it, the case fails regardless of the overall"}
        }
    }
}"#;

/// `create_benchmark.dataset` — inline cases. Only `input` is required; `expected` (reference) and
/// `output` (candidate to judge) are optional.
pub(crate) const BENCHMARK_DATASET: &str = r#"{
    "type": "array",
    "description": "inline benchmark cases",
    "items": {
        "type": "object",
        "required": ["input"],
        "properties": {
            "input": {"type":"string","description":"the case prompt / input"},
            "expected": {"type":"string","description":"golden reference answer the judge can compare against (optional)"},
            "output": {"type":"string","description":"a pre-captured candidate response to judge; omit to generate from targets (optional)"},
            "difficulty": {"type":"string","enum":["easy","medium","hard"],"description":"how hard this case is meant to be, on a closed ordered ladder. Omit for ungraded - ungraded is a state of its own, never 'medium'. A spelling off this ladder is a 400, not a silent downgrade, so grade deliberately or leave it out. Grading is what lets a run report which tier actually separated the targets: a tier every target passes measured nothing and is money spent to learn nothing (optional)"}
        }
    }
}"#;

/// `create_benchmark.targets` — the provider/model comparison matrix. `provider` + `model` are
/// required; `system_prompt` (the variant under test) and `label` optional. Note the field is
/// `system_prompt`, not `prompt`.
pub(crate) const BENCHMARK_TARGETS: &str = r#"{
    "type": "array",
    "description": "comparison matrix: one row per target under test. A target is a (provider, model) with either a literal system_prompt or a prompt_ref that resolves from the registry at run time, or an HTTP endpoint of your own (kind.type=http) that receives {input, expected?, system_prompt?} and returns {output, usage?, latency_ms?, cost_usd?}.",
    "items": {
        "type": "object",
        "required": ["provider", "model"],
        "properties": {
            "provider": {"type":"string","description":"e.g. anthropic, openai; for an http target, a label for whatever answers there"},
            "model": {"type":"string"},
            "system_prompt": {"type":"string","description":"literal system/instruction prompt variant under test"},
            "label": {"type":"string","description":"display label; defaults to provider/model, or provider/model@effort when an effort is set"},
            "effort": {"type":"string","enum":["low","medium","high","xhigh","max"],"description":"reasoning effort for this target. THIS IS AN AXIS: the same model at two efforts is two commensurable rows, which is how you ask whether the expensive setting buys anything on your cases. Omit for the provider's default - an absent effort is a different fact from any named level. Not every adapter can honour every level; one that cannot refuses loudly rather than silently sending the default (optional)"},
            "prompt_ref": {
                "type": "object",
                "description": "resolve this target's prompt from the registry at run start instead of using the literal system_prompt. Required for the promotion gate to certify the version it ran: a run only reports resolved_prompt_version when it fetched the content. Pass at most one of version/label.",
                "required": ["name"],
                "properties": {
                    "name": {"type":"string","description":"registry name, must already exist in this project"},
                    "version": {"type":"integer","description":"pin an exact version"},
                    "label": {"type":"string","description":"resolve through a label, e.g. production"}
                }
            },
            "limits": {
                "type": "object",
                "description": "per-case service ceilings this target must hold to BESIDES scoring well. A case whose generation exceeds one FAILS, however well the judge scored it - which is the part a rubric cannot express: the same answer at 8s and 4c a case is not the same product as one at 1s and a tenth of a cent. A ceiling that could not be checked (an unpriced model has no cost) is reported as unchecked, never as a pass. Compare mode only (optional).",
                "properties": {
                    "max_cost_usd": {"type":"number","description":"most one case's generation may cost, USD per candidate - so a --gen-samples 3 run is held to the same per-call bar, not three times it"},
                    "max_latency_ms": {"type":"integer","description":"longest one case's generation may take, milliseconds per candidate"}
                }
            },
            "kind": {
                "type": "object",
                "description": "how this target produces output. Omit for a model call.",
                "required": ["type"],
                "properties": {
                    "type": {"type":"string","enum":["model","http"]},
                    "url": {"type":"string","description":"https endpoint (http target only). Private, loopback and link-local addresses are refused."}
                }
            }
        }
    }
}"#;

#[cfg(test)]
mod tests {
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
}
