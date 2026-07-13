//! Full JSON schemas for the write tools' nested parameters. These mirror exactly what the API
//! deserializes — `RubricDimension` (crates/core/src/rubric.rs), and `BenchmarkCase` / `BenchTarget`
//! (crates/core/src/score.rs) — so an agent fills the right field names instead of guessing from prose.

use serde_json::{json, Value};

/// `create_rubric.dimensions` — array of weighted, anchored scoring dimensions. `key` + `description`
/// are required (neither has a serde default); `weight`/`anchors`/`floor` are optional.
pub(crate) fn rubric_dimensions() -> Value {
    json!({
        "type": "array",
        "description": "weighted scoring dimensions the judge fills per case",
        "items": {
            "type": "object",
            "required": ["key", "description"],
            "properties": {
                "key": {"type":"string","description":"stable identifier used in the judge's JSON output (e.g. correctness)"},
                "description": {"type":"string","description":"what this dimension measures"},
                "weight": {"type":"number","description":"relative weight in the overall score (default 1.0)"},
                "anchors": {"type":"array","items":{"type":"string"},
                    "description":"anchored level descriptions, e.g. [\"1.0 = fully correct\", \"0.5 = minor error\", \"0 = wrong\"]"},
                "floor": {"type":"number","description":"gating floor: if this dimension scores below it, the case fails regardless of the overall"}
            }
        }
    })
}

/// `create_benchmark.dataset` — inline cases. Only `input` is required; `expected` (reference) and
/// `output` (candidate to judge) are optional.
pub(crate) fn benchmark_dataset() -> Value {
    json!({
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
    })
}

/// `create_benchmark.targets` — the provider/model comparison matrix. `provider` + `model` required;
/// `system_prompt` (the variant under test) and `label` optional. Note the field is `system_prompt`,
/// not `prompt`.
pub(crate) fn benchmark_targets() -> Value {
    json!({
        "type": "array",
        "description": "comparison matrix: one row per (provider, model[, system prompt]) under test",
        "items": {
            "type": "object",
            "required": ["provider", "model"],
            "properties": {
                "provider": {"type":"string","description":"e.g. anthropic, openai"},
                "model": {"type":"string"},
                "system_prompt": {"type":"string","description":"system/instruction prompt variant under test"},
                "label": {"type":"string","description":"display label; defaults to provider/model"}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_required(schema: &Value) -> Vec<String> {
        schema["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn rubric_dimensions_require_key_and_description() {
        let s = rubric_dimensions();
        assert_eq!(item_required(&s), vec!["key", "description"]);
        let props = &s["items"]["properties"];
        for f in ["key", "description", "weight", "anchors", "floor"] {
            assert!(props.get(f).is_some(), "missing {f}");
        }
        assert_eq!(s["items"]["properties"]["anchors"]["items"]["type"], "string");
    }

    #[test]
    fn benchmark_dataset_requires_only_input() {
        let s = benchmark_dataset();
        assert_eq!(item_required(&s), vec!["input"]);
        for f in ["input", "expected", "output"] {
            assert!(s["items"]["properties"].get(f).is_some(), "missing {f}");
        }
    }

    #[test]
    fn benchmark_targets_use_system_prompt_not_prompt() {
        let s = benchmark_targets();
        assert_eq!(item_required(&s), vec!["provider", "model"]);
        let props = &s["items"]["properties"];
        assert!(props.get("system_prompt").is_some());
        assert!(props.get("prompt").is_none(), "the real field is system_prompt");
        assert!(props.get("label").is_some());
    }
}
