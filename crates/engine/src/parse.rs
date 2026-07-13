//! JSON extraction from model text plus the one-shot *repair re-ask* wrapped around a single judge
//! sample. A sample is a logical unit (1–2 provider calls); [`sample_parsed`] does its own accounting
//! so callers fold results deterministically, and it never scores a phantom 0.0 — an unparseable
//! sample surfaces as `value: None` with the raw text preserved.

use serde_json::Value;

use crate::prompts::build_repair_prompt;
use crate::{GenOutcome, Result};

/// Extract the outermost `{...}` from a string (handles stray prose / code fences).
pub(crate) fn extract_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| s[start..=end].to_string())
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

    // Repair re-ask: hand the bad output back and demand strict JSON, exactly once.
    let repair = build_repair_prompt(prompt, &malformed);
    match gen(index, &repair) {
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
