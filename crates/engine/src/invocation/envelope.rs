//! Reading the `--output-format json` result envelope. Pure functions over a [`Value`] — no
//! process, no I/O — so the parsing every caller depends on is testable without a `claude` on PATH.

use serde_json::Value;

/// The completion text. With `--json-schema` the model's structured answer lands in
/// `structured_output` (an object) — prefer it, serialized, so downstream JSON extraction sees clean
/// JSON; otherwise fall back to the free-text `result`.
pub(crate) fn completion_text(envelope: &Value) -> String {
    if let Some(v) = structured(envelope) {
        return v.to_string();
    }
    envelope
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The structured answer, when the call asked for one and the CLI returned one.
pub(crate) fn structured(envelope: &Value) -> Option<&Value> {
    envelope.get("structured_output").filter(|v| !v.is_null())
}

/// Total (input, output) tokens from a claude `usage` block (input includes cache read + creation).
pub(crate) fn token_counts(envelope: &Value) -> (Option<u64>, Option<u64>) {
    let usage = envelope.get("usage");
    let input = usage.map(|u| {
        let f = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        f("input_tokens") + f("cache_read_input_tokens") + f("cache_creation_input_tokens")
    });
    let output = usage.and_then(|u| u.get("output_tokens").and_then(Value::as_u64));
    (input, output)
}

/// Resolve the model name reported in the envelope, falling back to `fallback`.
pub(crate) fn model_of(envelope: &Value, fallback: &str) -> String {
    envelope
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

/// Split a trailing `@<effort>` (low|medium|high|xhigh|max) off a model spec, e.g.
/// "opus@xhigh" → ("opus", Some("xhigh")). Any other string → (model, None).
pub(crate) fn split_effort(model: &str) -> (&str, Option<&str>) {
    if let Some((m, e)) = model.rsplit_once('@') {
        if matches!(e, "low" | "medium" | "high" | "xhigh" | "max") {
            return (m, Some(e));
        }
    }
    (model, None)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn completion_text_prefers_the_structured_answer() {
        let env = json!({ "result": "prose", "structured_output": { "score": 1 } });
        assert_eq!(completion_text(&env), "{\"score\":1}");
        assert_eq!(structured(&env), Some(&json!({ "score": 1 })));

        let env = json!({ "result": "prose", "structured_output": Value::Null });
        assert_eq!(completion_text(&env), "prose");
        assert!(structured(&env).is_none());

        assert_eq!(completion_text(&json!({})), "");
    }

    #[test]
    fn token_counts_fold_the_cache_buckets_into_input() {
        let env = json!({ "usage": {
            "input_tokens": 10, "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 2, "output_tokens": 7
        }});
        assert_eq!(token_counts(&env), (Some(17), Some(7)));
        assert_eq!(token_counts(&json!({ "usage": {} })), (Some(0), None));
        assert_eq!(token_counts(&json!({})), (None, None));
    }

    #[test]
    fn model_of_falls_back_to_the_requested_model() {
        assert_eq!(
            model_of(&json!({ "model": "claude-x" }), "haiku"),
            "claude-x"
        );
        assert_eq!(model_of(&json!({}), "haiku"), "haiku");
    }

    #[test]
    fn split_effort_only_splits_known_levels() {
        assert_eq!(split_effort("opus@xhigh"), ("opus", Some("xhigh")));
        assert_eq!(split_effort("sonnet"), ("sonnet", None));
        assert_eq!(split_effort("weird@thing"), ("weird@thing", None));
    }
}
