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
            "output": {"type":"string","description":"a pre-captured candidate response to judge; omit to generate from targets (optional)"}
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
            "label": {"type":"string","description":"display label; defaults to provider/model"},
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
