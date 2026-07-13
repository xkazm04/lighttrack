//! Judge / evaluation / rubric prompt and schema builders.

use serde_json::{json, Map, Value};

use lighttrack_core::Rubric;

/// Build a judging prompt for an input/output pair against a freeform rubric.
pub fn build_judge_prompt(rubric: &str, input: &str, output: &str) -> String {
    format!(
        "You are a strict evaluation judge. Evaluate the ASSISTANT OUTPUT for the given USER INPUT \
against the rubric below.\n\
Rubric: {rubric}\n\n\
Respond with ONLY a JSON object (no prose, no code fences) of the form:\n\
{{\"score\": <number 0.0-1.0>, \"max\": 1.0, \"pass\": <true|false>, \"reasoning\": \"<one sentence>\"}}\n\n\
=== USER INPUT ===\n{input}\n\n=== ASSISTANT OUTPUT ===\n{output}\n"
    )
}

/// Build a benchmark eval prompt for an input/output pair, with an optional reference answer.
pub fn build_eval_prompt(rubric: &str, input: &str, expected: Option<&str>, output: &str) -> String {
    let reference = match expected {
        Some(e) => format!("\n=== REFERENCE / EXPECTED ANSWER ===\n{e}\n"),
        None => String::new(),
    };
    format!(
        "You are a strict evaluation judge. Evaluate the ASSISTANT OUTPUT for the given USER INPUT \
against the rubric{ref_note}.\n\
Rubric: {rubric}\n\n\
Respond with ONLY a JSON object (no prose, no code fences):\n\
{{\"score\": <number 0.0-1.0>, \"max\": 1.0, \"pass\": <true|false>, \"reasoning\": \"<one sentence>\"}}\n\n\
=== USER INPUT ===\n{input}\n{reference}\n=== ASSISTANT OUTPUT ===\n{output}\n",
        ref_note = if expected.is_some() {
            " and the reference answer"
        } else {
            ""
        }
    )
}

/// One-shot repair re-ask: the judge returned unparseable output, so hand the malformed text back and
/// demand strict JSON. The marker phrase "ONLY valid JSON matching the schema" is stable so tests (and
/// humans) can recognise a repair call.
pub(crate) fn build_repair_prompt(original: &str, malformed: &str) -> String {
    format!(
        "{original}\n\n=== YOUR PREVIOUS RESPONSE (rejected) ===\n{malformed}\n\n\
The response above was not valid JSON in the required shape. Return ONLY valid JSON matching the \
schema — no prose, no code fences, no commentary before or after."
    )
}

/// Build a JSON schema keyed by dimension: each dimension yields `{score, reasoning}`.
pub fn build_rubric_schema(rubric: &Rubric) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for d in &rubric.dimensions {
        props.insert(
            d.key.clone(),
            json!({
                "type": "object",
                "properties": {
                    "score": { "type": "number", "description": format!("0.0-1.0 — {}", d.description) },
                    "reasoning": { "type": "string" }
                },
                "required": ["score", "reasoning"],
                "additionalProperties": false
            }),
        );
        required.push(Value::String(d.key.clone()));
    }
    let mut root = Map::new();
    root.insert("type".into(), json!("object"));
    root.insert("properties".into(), Value::Object(props));
    root.insert("required".into(), Value::Array(required));
    root.insert("additionalProperties".into(), json!(false));
    Value::Object(root)
}

/// Pairwise preference prompt: judge which of two answers (A / B) is better for the input, or a tie.
/// The judge is told explicitly to weigh *content* only — never style, length, formatting, or which
/// system produced an answer — and that ordering carries no meaning (the caller counterbalances A/B).
pub fn build_pairwise_prompt(
    input: &str,
    expected: Option<&str>,
    answer_a: &str,
    answer_b: &str,
    criteria: Option<&str>,
) -> String {
    let reference = expected
        .map(|e| format!("\n=== REFERENCE / EXPECTED ===\n{e}\n"))
        .unwrap_or_default();
    let crit = criteria
        .filter(|c| !c.is_empty())
        .map(|c| format!("\nJudge against these criteria: {c}\n"))
        .unwrap_or_default();
    format!(
        "You are an impartial preference judge. Two answers (A and B) respond to the same input. Decide \
which answer is better on the MERIT OF ITS CONTENT for the input{ref_note}. Judge correctness and \
usefulness only — ignore style, tone, length, formatting, and which system produced an answer; do NOT \
prefer an answer merely for being longer or more verbose. The A/B ordering is arbitrary and must not \
influence you. If they are equally good (or equally bad), answer \"Tie\".{crit}\n\
Return ONLY a JSON object: {{\"winner\": \"A\" | \"B\" | \"Tie\", \"reasoning\": \"<one sentence>\"}}.\n\n\
=== INPUT ===\n{input}\n{reference}\n=== ANSWER A ===\n{answer_a}\n\n=== ANSWER B ===\n{answer_b}\n",
        ref_note = if expected.is_some() { " and the reference" } else { "" }
    )
}

/// JSON schema for a [`PairwiseVerdict`](crate::PairwiseVerdict): `{winner: A|B|Tie, reasoning}`.
pub(crate) fn build_pairwise_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "winner": { "type": "string", "enum": ["A", "B", "Tie"] },
            "reasoning": { "type": "string" }
        },
        "required": ["winner", "reasoning"],
        "additionalProperties": false
    })
}

/// RCAF judge prompt for a rubric: Role, Context (dimensions+anchors+reference), Action, Format.
pub fn build_rubric_prompt(
    rubric: &Rubric,
    input: &str,
    expected: Option<&str>,
    output: &str,
) -> String {
    let dims = rubric
        .dimensions
        .iter()
        .map(|d| {
            let anchors = if d.anchors.is_empty() {
                String::new()
            } else {
                format!(" Anchors: {}", d.anchors.join("; "))
            };
            format!("- {} (weight {}): {}.{}", d.key, d.weight, d.description, anchors)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let reference = expected
        .map(|e| format!("\n=== REFERENCE / EXPECTED ===\n{e}\n"))
        .unwrap_or_default();
    format!(
        "You are an impartial, strict evaluation judge. Score the ASSISTANT OUTPUT on EACH dimension \
below from 0.0 to 1.0 using the anchors. Penalize unnecessary length; do not reward verbosity. Judge \
only the output's quality for the input{ref_note}; ignore which model produced it.\n\n\
Dimensions:\n{dims}\n\n\
Return ONLY a JSON object mapping each dimension key to {{\"score\": <0.0-1.0>, \"reasoning\": \"<one sentence>\"}}.\n\n\
=== USER INPUT ===\n{input}\n{reference}\n=== ASSISTANT OUTPUT ===\n{output}\n",
        ref_note = if expected.is_some() { " and the reference" } else { "" }
    )
}
