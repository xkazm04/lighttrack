//! Judge / evaluation / rubric prompt and schema builders.
//!
//! Every builder returns a [`Prompt`]: the rendered text plus an *injection-suspected* signal raised
//! when the untrusted content it fenced tried to imitate a section boundary (see [`crate::fence`]).
//! Rubric text and judge criteria are operator-authored and stay outside the fence; the input,
//! reference and candidate output are attacker-controlled and are always fenced.

use serde_json::{json, Map, Value};

use lighttrack_core::Rubric;

use crate::fence::Fence;

/// A built judge prompt. Derefs to `str`, so it passes anywhere the raw prompt text is wanted.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub text: String,
    /// Untrusted content in this prompt imitated a section/boundary marker and was neutralized.
    pub injection_suspected: bool,
}

impl Prompt {
    /// A prompt with no untrusted content — the scoring math's fixtures, which need no fence.
    #[cfg(test)]
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Prompt { text: text.into(), injection_suspected: false }
    }

    fn fenced(text: String, fence: &Fence) -> Self {
        Prompt { text, injection_suspected: fence.injection_suspected() }
    }
}

impl std::ops::Deref for Prompt {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Build a judging prompt for an input/output pair against a freeform rubric.
pub fn build_judge_prompt(rubric: &str, input: &str, output: &str) -> Prompt {
    let mut fence = Fence::new();
    let blocks = format!(
        "{}{}",
        fence.wrap("USER INPUT", input),
        fence.wrap("ASSISTANT OUTPUT", output)
    );
    let text = format!(
        "You are a strict evaluation judge. Evaluate the ASSISTANT OUTPUT for the given USER INPUT \
against the rubric below.\n\
{preamble}\n\
Rubric: {rubric}\n\n\
Respond with ONLY a JSON object (no prose, no code fences) of the form:\n\
{{\"score\": <number 0.0-1.0>, \"max\": 1.0, \"pass\": <true|false>, \"reasoning\": \"<one sentence>\"}}\n\n\
{blocks}",
        preamble = fence.preamble()
    );
    Prompt::fenced(text, &fence)
}

/// Build a benchmark eval prompt for an input/output pair, with an optional reference answer.
pub fn build_eval_prompt(
    rubric: &str,
    input: &str,
    expected: Option<&str>,
    output: &str,
) -> Prompt {
    let mut fence = Fence::new();
    let input_block = fence.wrap("USER INPUT", input);
    let reference = expected
        .map(|e| fence.wrap("REFERENCE / EXPECTED ANSWER", e))
        .unwrap_or_default();
    let output_block = fence.wrap("ASSISTANT OUTPUT", output);
    let text = format!(
        "You are a strict evaluation judge. Evaluate the ASSISTANT OUTPUT for the given USER INPUT \
against the rubric{ref_note}.\n\
{preamble}\n\
Rubric: {rubric}\n\n\
Respond with ONLY a JSON object (no prose, no code fences):\n\
{{\"score\": <number 0.0-1.0>, \"max\": 1.0, \"pass\": <true|false>, \"reasoning\": \"<one sentence>\"}}\n\n\
{input_block}{reference}{output_block}",
        preamble = fence.preamble(),
        ref_note = if expected.is_some() {
            " and the reference answer"
        } else {
            ""
        }
    );
    Prompt::fenced(text, &fence)
}

/// One-shot repair re-ask: the judge returned unparseable output, so hand the malformed text back and
/// demand strict JSON. The marker phrase "ONLY valid JSON matching the schema" is stable so tests (and
/// humans) can recognise a repair call. The malformed text is *model* output — the widest injection
/// surface of all, since a compromised candidate may have talked the judge into echoing it — so it is
/// fenced under a fresh nonce, which also neutralizes any boundary marker echoed from `original`.
pub(crate) fn build_repair_prompt(original: &str, malformed: &str) -> Prompt {
    let mut fence = Fence::new();
    let block = fence.wrap("YOUR PREVIOUS RESPONSE (rejected)", malformed);
    let text = format!(
        "{original}\n\n{preamble}\n{block}\n\
The response above was not valid JSON in the required shape. Return ONLY valid JSON matching the \
schema — no prose, no code fences, no commentary before or after.",
        preamble = fence.preamble()
    );
    Prompt::fenced(text, &fence)
}

/// Build a JSON schema keyed by dimension: each dimension yields `{score, reasoning}`. Only
/// LLM-judged dimensions appear — a deterministic dimension is checked locally, so asking the model
/// for it would both waste tokens and let its opinion double-count against the mechanical verdict.
pub fn build_rubric_schema(rubric: &Rubric) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for d in rubric.dimensions.iter().filter(|d| d.kind.is_llm()) {
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
) -> Prompt {
    let mut fence = Fence::new();
    let input_block = fence.wrap("INPUT", input);
    let reference = expected
        .map(|e| fence.wrap("REFERENCE / EXPECTED", e))
        .unwrap_or_default();
    let a_block = fence.wrap("ANSWER A", answer_a);
    let b_block = fence.wrap("ANSWER B", answer_b);
    let crit = criteria
        .filter(|c| !c.is_empty())
        .map(|c| format!("\nJudge against these criteria: {c}\n"))
        .unwrap_or_default();
    let text = format!(
        "You are an impartial preference judge. Two answers (A and B) respond to the same input. Decide \
which answer is better on the MERIT OF ITS CONTENT for the input{ref_note}. Judge correctness and \
usefulness only — ignore style, tone, length, formatting, and which system produced an answer; do NOT \
prefer an answer merely for being longer or more verbose. The A/B ordering is arbitrary and must not \
influence you. If they are equally good (or equally bad), answer \"Tie\".{crit}\n\
{preamble}\n\
Return ONLY a JSON object: {{\"winner\": \"A\" | \"B\" | \"Tie\", \"reasoning\": \"<one sentence>\"}}.\n\n\
{input_block}{reference}{a_block}{b_block}",
        preamble = fence.preamble(),
        ref_note = if expected.is_some() { " and the reference" } else { "" }
    );
    Prompt::fenced(text, &fence)
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
///
/// Only the rubric's LLM-judged dimensions are narrated. Deterministic dimensions
/// ([`lighttrack_core::DimensionKind`]) are evaluated locally and are deliberately invisible here, so
/// the model never scores — and so never double-counts — a check the engine already decided.
pub fn build_rubric_prompt(
    rubric: &Rubric,
    input: &str,
    expected: Option<&str>,
    output: &str,
) -> Prompt {
    let dims = rubric
        .dimensions
        .iter()
        .filter(|d| d.kind.is_llm())
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
    let mut fence = Fence::new();
    let input_block = fence.wrap("USER INPUT", input);
    let reference = expected
        .map(|e| fence.wrap("REFERENCE / EXPECTED", e))
        .unwrap_or_default();
    let output_block = fence.wrap("ASSISTANT OUTPUT", output);
    let text = format!(
        "You are an impartial, strict evaluation judge. Score the ASSISTANT OUTPUT on EACH dimension \
below from 0.0 to 1.0 using the anchors. Penalize unnecessary length; do not reward verbosity. Judge \
only the output's quality for the input{ref_note}; ignore which model produced it.\n\n\
{preamble}\n\
Dimensions:\n{dims}\n\n\
Return ONLY a JSON object mapping each dimension key to {{\"score\": <0.0-1.0>, \"reasoning\": \"<one sentence>\"}}.\n\n\
{input_block}{reference}{output_block}",
        preamble = fence.preamble(),
        ref_note = if expected.is_some() { " and the reference" } else { "" }
    );
    Prompt::fenced(text, &fence)
}

#[cfg(test)]
mod tests;
