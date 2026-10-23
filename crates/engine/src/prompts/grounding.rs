//! Prompts and schemas for the `grounding` dimension kind: the cut (decompose the output into
//! standalone claims) and the check (verify each claim against the evidence passages).
//!
//! The instruction text here is part of the instrument. It is fingerprinted into [`instrument_pin`],
//! which every grounding verdict carries, so a score cut at one granularity is never silently trended
//! against another.

use serde_json::{json, Value};

use super::Prompt;
use crate::fence::Fence;

/// The cutter's instruction. Editing it changes what a grounding score means, and moves the pin.
pub(crate) const DECOMPOSE_INSTRUCTION: &str = "Rewrite the ASSISTANT OUTPUT below as a list of standalone \
claims. Each claim is ONE assertion that can be checked with no other part of the output in view: \
resolve pronouns and references inside the claim. Cover EVERY assertion the output makes, including \
trailing qualifiers, list items and attributes. Do not select, summarize, judge, correct, or add \
anything the output does not say. Greetings, questions, refusals and instructions are not claims; \
an output that asserts nothing checkable yields an empty list.";

/// The verifier's instruction. Binary per claim: partial support is expressed by cutting finer.
pub(crate) const VERIFY_INSTRUCTION: &str = "For EACH numbered CLAIM below, decide whether the EVIDENCE \
PASSAGES entail it. A claim is supported only if the passages state it or directly imply it. A claim \
the passages do not mention is unsupported, exactly like one they contradict. Judge entailment, not \
plausibility, and use no knowledge beyond the passages.";

/// Bump when the procedure's shape changes (schemas, the scoring rule). Instruction text is
/// fingerprinted on its own, so a reworded instruction cannot keep an old pin.
const PROCEDURE_REVISION: u32 = 1;

/// The pin stamped on every grounding verdict: procedure revision plus an FNV-1a fingerprint of both
/// instructions. FNV rather than `DefaultHasher`, whose output is not stable across Rust releases.
pub(crate) fn instrument_pin() -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in DECOMPOSE_INSTRUCTION
        .bytes()
        .chain(VERIFY_INSTRUCTION.bytes())
    {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    format!("grounding-v{PROCEDURE_REVISION}-{h:08x}")
}

pub(crate) fn decompose_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "claims": { "type": "array", "items": { "type": "string" } } },
        "required": ["claims"],
        "additionalProperties": false
    })
}

pub(crate) fn verify_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "verdicts": { "type": "array", "items": {
            "type": "object",
            "properties": {
                "claim": { "type": "integer", "description": "the number from the CLAIM heading" },
                "supported": { "type": "boolean" },
                "reason": { "type": "string" }
            },
            "required": ["claim", "supported", "reason"],
            "additionalProperties": false
        } } },
        "required": ["verdicts"],
        "additionalProperties": false
    })
}

/// The cut reads the output alone: given the question, a cutter can quietly omit what it judges off
/// topic, and an omitted claim can never be marked unsupported.
pub(crate) fn decompose_prompt(output: &str) -> Prompt {
    let mut fence = Fence::new();
    let block = fence.wrap("ASSISTANT OUTPUT", output);
    let text = format!(
        "{DECOMPOSE_INSTRUCTION}\n\n{preamble}\nReturn ONLY a JSON object: \
         {{\"claims\": [\"<claim>\", ...]}}.\n\n{block}",
        preamble = fence.preamble()
    );
    Prompt {
        injection_suspected: fence.injection_suspected(),
        text,
    }
}

/// Every passage and every claim is fenced separately: a claim is candidate text rewritten by a
/// model, and one passage must not be able to close or impersonate another.
pub(crate) fn verify_prompt(evidence: &[String], claims: &[String]) -> Prompt {
    let mut fence = Fence::new();
    let mut blocks = String::new();
    for (i, p) in evidence.iter().enumerate() {
        blocks.push_str(&fence.wrap(&format!("EVIDENCE PASSAGE {}", i + 1), p));
    }
    if evidence.is_empty() {
        blocks.push_str("(No evidence passages were supplied.)\n");
    }
    for (i, c) in claims.iter().enumerate() {
        blocks.push_str(&fence.wrap(&format!("CLAIM {}", i + 1), c));
    }
    let text = format!(
        "{VERIFY_INSTRUCTION}\n\n{preamble}\nReturn ONLY a JSON object {{\"verdicts\": [{{\"claim\": \
         <number>, \"supported\": <true|false>, \"reason\": \"<one sentence>\"}}, ...]}} with EXACTLY \
         {n} entries, one per CLAIM, each carrying the number from its CLAIM heading.\n\n{blocks}",
        n = claims.len(),
        preamble = fence.preamble()
    );
    Prompt {
        injection_suspected: fence.injection_suspected(),
        text,
    }
}
