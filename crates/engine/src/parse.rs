//! JSON extraction from model text plus the one-shot *repair re-ask* wrapped around a single judge
//! sample. A sample is a logical unit (1–2 provider calls); [`sample_parsed`] does its own accounting
//! so callers fold results deterministically, and it never scores a phantom 0.0 — an unparseable
//! sample surfaces as `value: None` with the raw text preserved.

use serde_json::Value;

use crate::prompts::build_repair_prompt;
use crate::{Determinism, GenOutcome, Result};

/// Extract the first balanced `{...}` that parses as JSON from a string (stray prose, code fences,
/// braces in the surrounding text).
///
/// The first-`{`-to-last-`}` slice this replaces failed whenever the model's prose mentioned a brace
/// before or after its verdict ("the shape {score, reasoning} is ... {\"score\":0.2}" — or a
/// trailing "}" in a sentence), and every such failure bought a paid repair re-ask for output that
/// was in fact fine. A balanced scan that respects string literals finds the verdict wherever the
/// prose put it; the slice is the fallback when nothing balanced parses.
pub(crate) fn extract_json_object(s: &str) -> Option<String> {
    for (start, _) in s.match_indices('{') {
        if let Some(end) = balanced_end(&s[start..]) {
            let candidate = &s[start..start + end];
            if serde_json::from_str::<Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| s[start..=end].to_string())
}

/// Byte length of the balanced `{...}` at the start of `s`, honouring JSON string literals (a brace
/// inside `"..."` does not count). `None` when the object never closes.
fn balanced_end(s: &str) -> Option<usize> {
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    for (i, b) in s.bytes().enumerate() {
        match (in_str, b) {
            (true, b'\\') if !escaped => escaped = true,
            (true, b'"') if !escaped => in_str = false,
            (true, _) => escaped = false,
            (false, b'"') => in_str = true,
            (false, b'{') => depth += 1,
            (false, b'}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract a JSON object from text into a Value (lenient; `Null` if none).
pub(crate) fn extract_json_value(s: &str) -> Value {
    extract_json_object(s)
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or(Value::Null)
}

/// The outcome of one judge sample: the parsed value (if any) plus cost/latency/token accounting for
/// *every* provider call the sample made (the first attempt and, if it happened, the repair re-ask).
pub(crate) struct Parsed<T> {
    /// `Some` iff the sample parsed (on the first try or after repair); `None` if it stayed bad.
    pub(crate) value: Option<T>,
    /// The last unparseable raw output, kept so an all-failed run can report *why*.
    pub(crate) raw_failure: Option<String>,
    /// The repair re-ask re-embedded model text that imitated a prompt boundary (see [`crate::fence`]).
    pub(crate) injection_suspected: bool,
    /// The weakest determinism across this sample's calls — a repaired sample is only as
    /// reproducible as its least reproducible attempt.
    pub(crate) determinism: Determinism,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) latency_ms: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) model: String,
}

impl<T> Parsed<T> {
    fn empty() -> Self {
        Parsed {
            value: None,
            raw_failure: None,
            injection_suspected: false,
            determinism: Determinism::Exact,
            cost_usd: None,
            latency_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            model: String::new(),
        }
    }

    /// Fold one provider call's cost/latency/tokens in. A repaired sample cost two calls; both are
    /// counted so the judge's true expense is never under-reported.
    fn record(&mut self, g: &GenOutcome) {
        if let Some(c) = g.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
        }
        self.latency_ms = self.latency_ms.max(g.latency_ms.unwrap_or(0));
        self.input_tokens += g.input_tokens.unwrap_or(0);
        self.output_tokens += g.output_tokens.unwrap_or(0);
        self.determinism = self.determinism.weakest(g.determinism);
        self.model = g.model.clone();
    }
}

/// Run one judge sample with a single repair re-ask. `gen(index, prompt)` performs one already-retried
/// generation; `parse` turns raw text into `T`. On empty/unparseable output we re-prompt *once* with
/// the malformed text and a demand for strict JSON. Hard, non-recoverable errors (auth, spawn, unknown
/// provider) abort by propagating; a sample still unparseable after repair returns `value: None` (a
/// dropped, honestly-counted failure) rather than a fabricated score.
pub(crate) fn sample_parsed<T>(
    gen: impl Fn(usize, &str) -> Result<GenOutcome>,
    index: usize,
    prompt: &str,
    parse: impl Fn(&str) -> Result<T>,
) -> Result<Parsed<T>> {
    let mut acc = Parsed::empty();

    // First attempt: parse cleanly, or capture the malformed text for the repair re-ask.
    let malformed = match gen(index, prompt) {
        Ok(g) => {
            acc.record(&g);
            match parse(&g.output) {
                Ok(v) => {
                    acc.value = Some(v);
                    return Ok(acc);
                }
                Err(_) => g.output,
            }
        }
        Err(e) if e.is_empty_completion() => String::new(),
        Err(e) => return Err(e),
    };

    // Repair re-ask: hand the bad output back and demand strict JSON, exactly once. The malformed
    // text is fenced under a fresh nonce, so a candidate that talked the judge into echoing an
    // injection cannot widen the surface on the second pass.
    let repair = build_repair_prompt(prompt, &malformed);
    acc.injection_suspected |= repair.injection_suspected;
    match gen(index, &repair.text) {
        Ok(g) => {
            acc.record(&g);
            match parse(&g.output) {
                Ok(v) => acc.value = Some(v),
                Err(_) => acc.raw_failure = Some(g.output),
            }
        }
        Err(e) if e.is_empty_completion() => acc.raw_failure = Some(malformed.clone()),
        Err(e) => return Err(e),
    }
    if acc.value.is_none() && acc.raw_failure.is_none() {
        acc.raw_failure = Some(malformed);
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::extract_json_object;

    /// A brace in the prose around the verdict must not turn a good verdict into a repair re-ask.
    #[test]
    fn a_verdict_is_found_despite_braces_in_the_surrounding_prose() {
        let before =
            "The required shape is {score, reasoning}. {\"score\":0.2,\"reasoning\":\"x\"}";
        assert_eq!(
            extract_json_object(before).as_deref(),
            Some("{\"score\":0.2,\"reasoning\":\"x\"}")
        );
        let after = "{\"score\":0.9,\"reasoning\":\"fine\"} and that closes the case }";
        assert_eq!(
            extract_json_object(after).as_deref(),
            Some("{\"score\":0.9,\"reasoning\":\"fine\"}")
        );
        // A brace inside a JSON string is content, not structure.
        let nested = "{\"reasoning\":\"uses {curly} text\",\"score\":1}";
        assert_eq!(extract_json_object(nested).as_deref(), Some(nested));
        // Nothing balanced parses: the old outermost slice is still handed to the repair path.
        assert_eq!(
            extract_json_object("{not json} trailing }").as_deref(),
            Some("{not json} trailing }")
        );
        assert_eq!(extract_json_object("no braces"), None);
    }
}
