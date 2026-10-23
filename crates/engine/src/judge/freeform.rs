//! The **freeform** judge contract: criteria text that carries an identity and a pass threshold.
//!
//! The structured-rubric path ([`crate::run_rubric_judge`]) computes `pass` locally, measures
//! agreement across the samples that parsed, and counts the ones that did not. The freeform path did
//! none of that: it persisted the judge model's own `"pass"` boolean, reported agreement as the
//! literal `1.0`, and turned a single unparseable sample into a hard error that killed a whole run.
//!
//! This module gives the freeform path the same shape as the rubric path: **the contract decides
//! pass** (`overall >= threshold`, computed here, never the model's boolean); the counters are
//! measurements over the samples that actually ran, so the `needs_review` triage reasons can fire on
//! a freeform verdict at all; the criteria carry a content-digest identity, so two runs under edited
//! criteria are distinguishable by query; and one bad sample is a counted drop, while *no* parseable
//! sample is still an error — exactly as `aggregate` refuses to emit a phantom 0.0.
//!
//! It lives beside `judge.rs` rather than in it: that file is already far past the ~300 LOC ceiling,
//! so the freeform aggregation is a sibling module and `judge.rs` only lost code.

use lighttrack_core::{judge_verdict_schema, JudgeVerdict, DEFAULT_RUBRIC_THRESHOLD};
use sha2::{Digest, Sha256};

use crate::parse::{extract_json_object, sample_parsed, Parsed};
use crate::pool;
use crate::prompts::Prompt;
use crate::{Determinism, EngineConfig, EngineError, JudgeOutcome, Result};

use super::{provider_gen, Generator};

/// The pass threshold a freeform contract gets when the operator set none. Deliberately the *same*
/// constant the structured rubric defaults to: two judge contracts that gate at different bars by
/// accident of which door a score came through would be the deviation this module exists to close.
pub const DEFAULT_FREEFORM_THRESHOLD: f64 = DEFAULT_RUBRIC_THRESHOLD;

/// Criteria text promoted to a contract: what to judge, the bar to clear, and a stable identity.
///
/// The identity is a digest of the criteria, not a name: freeform criteria arrive as a string on
/// every request, so there is no stored row to cite. A digest still answers what a stored `rubric_id`
/// answers — "same instrument?" — and changes the moment anybody edits a word of the criteria.
#[derive(Debug, Clone, PartialEq)]
pub struct FreeformContract {
    criteria: String,
    threshold: f64,
    id: String,
}

impl FreeformContract {
    /// A contract over `criteria` gating at [`DEFAULT_FREEFORM_THRESHOLD`].
    pub fn new(criteria: impl Into<String>) -> Self {
        Self::with_threshold(criteria, DEFAULT_FREEFORM_THRESHOLD)
    }

    /// A contract over `criteria` gating at an operator-chosen `threshold`, clamped to 0..=1: a bar
    /// outside the scale a normalized verdict can reach is a configuration error, not a gate.
    pub fn with_threshold(criteria: impl Into<String>, threshold: f64) -> Self {
        let criteria = criteria.into();
        let id = freeform_id(&criteria);
        Self {
            criteria,
            threshold: threshold.clamp(0.0, 1.0),
            id,
        }
    }

    pub fn criteria(&self) -> &str {
        &self.criteria
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// The stable identity of these criteria, stamped onto every verdict as its `rubric_id`.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `freeform:<16 hex>` — a prefixed, truncated sha256 of the criteria text. Prefixed so a reader (and
/// a query) tells a derived identity apart from a stored rubric id; truncated because this is an
/// equality key for grouping verdicts, not a security boundary.
fn freeform_id(criteria: &str) -> String {
    let digest = Sha256::digest(criteria.as_bytes());
    let mut out = String::with_capacity(25);
    out.push_str("freeform:");
    for b in &digest[..8] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// One case judged under a [`FreeformContract`], aggregated over `samples` draws. Deliberately shaped
/// like [`crate::RubricOutcome`] rather than [`JudgeOutcome`]: the fields a stored verdict needs —
/// `pass`, `agreement`, `samples_parsed`, `parse_failures` — are the ones the old path dictated or
/// fabricated.
#[derive(Debug, Clone)]
pub struct FreeformOutcome {
    /// The model's own verdict (first parsed sample): its score, scale and prose.
    /// **`verdict.pass` is the model's claim and is never the gate** — read [`Self::pass`]. The claim
    /// is kept because the disagreement between the two is itself a signal worth having.
    pub verdict: JudgeVerdict,
    /// The normalized (`score / max`) mean over the samples that parsed.
    pub overall: f64,
    /// **The contract's decision**: `overall >= threshold`. Never `verdict.pass`.
    pub pass: bool,
    /// The threshold the decision above was taken at.
    pub threshold: f64,
    /// [`FreeformContract::id`] — what scored this case.
    pub contract_id: String,
    /// Samples requested.
    pub samples: u32,
    /// How many of them produced a usable verdict.
    pub samples_parsed: u32,
    /// How many were unparseable even after the one-shot repair re-ask, and so were dropped from the
    /// mean rather than scored 0.0.
    pub parse_failures: u32,
    /// Cross-sample agreement on the normalized score (`1 - spread`, clamped); a single surviving
    /// sample has nothing to disagree with and reports 1.0.
    pub agreement: f64,
    pub cost_usd: Option<f64>,
    pub model: String,
    pub latency_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub injection_suspected: bool,
    pub determinism: Determinism,
}

impl FreeformOutcome {
    /// Whether the model's self-reported `pass` contradicts the contract's decision — the cheapest
    /// signal that the criteria text and the threshold describe different bars.
    pub fn model_dissented(&self) -> bool {
        self.verdict.pass != self.pass
    }
}

/// Parse one judge sample's raw text into a [`JudgeVerdict`]: the single definition of what a freeform
/// judge reply has to be, shared by [`run_judge`] and [`run_freeform_judge`].
fn parse_verdict(raw: &str) -> Result<JudgeVerdict> {
    let json = extract_json_object(raw)
        .ok_or_else(|| EngineError::Parse(format!("no JSON object in judge output: {raw}")))?;
    serde_json::from_str::<JudgeVerdict>(&json)
        .map_err(|e| EngineError::Parse(format!("judge JSON not a verdict: {e}; got: {json}")))
}

/// Normalize onto 0..1. A `max` of zero (or worse) makes the ratio meaningless, so the raw score
/// stands rather than producing an infinity.
fn normalized(v: &JudgeVerdict) -> f64 {
    if v.max > 0.0 {
        v.score / v.max
    } else {
        v.score
    }
}

/// One freeform judge sample (one or two provider calls, the second being the repair re-ask).
fn freeform_sample(
    gen: &impl Generator,
    prompt: &str,
    index: usize,
) -> Result<Parsed<JudgeVerdict>> {
    sample_parsed(|i, p| gen.generate(i, p), index, prompt, parse_verdict)
}

/// Judge one input/output pair against freeform criteria: a single sample, the model's own verdict
/// verbatim. A caller that *stores* a verdict wants [`run_freeform_judge`] instead — this function
/// hands back `verdict.pass`, which is the model's claim and not a gate.
pub fn run_judge(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    prompt: &Prompt,
) -> Result<JudgeOutcome> {
    let schema = judge_verdict_schema();
    let gen = provider_gen(cfg, provider, model, schema, 1);
    let parsed = freeform_sample(&gen, &prompt.text, 0)?;
    let verdict = parsed.value.ok_or_else(|| {
        EngineError::Parse(
            parsed
                .raw_failure
                .map(|r| format!("judge output not a verdict after repair: {r}"))
                .unwrap_or_else(|| "judge produced no verdict".into()),
        )
    })?;
    Ok(JudgeOutcome {
        verdict,
        cost_usd: parsed.cost_usd,
        model: parsed.model,
        session_id: None,
        latency_ms: Some(parsed.latency_ms),
        input_tokens: Some(parsed.input_tokens),
        output_tokens: Some(parsed.output_tokens),
        injection_suspected: prompt.injection_suspected || parsed.injection_suspected,
        determinism: parsed.determinism,
    })
}

/// Judge one case under `contract`, drawing `samples` self-consistency draws (bounded by `jobs`) and
/// folding them into a contract-computed verdict. `prompt` is built by
/// [`crate::build_freeform_prompt`] from the same contract — the caller passes it so the
/// reference-answer block is decided once, by whoever knows whether the case has one.
pub fn run_freeform_judge(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    contract: &FreeformContract,
    prompt: &Prompt,
    samples: u32,
    jobs: usize,
) -> Result<FreeformOutcome> {
    let schema = judge_verdict_schema();
    let gen = provider_gen(cfg, provider, model, schema, samples);
    freeform_with(&gen, contract, prompt, model, samples, jobs)
}

/// [`run_freeform_judge`] over an injected [`Generator`] — the seam the unit tests drive, so the
/// pass/threshold, agreement and parse-failure math runs without a live provider call.
pub(crate) fn freeform_with(
    gen: &impl Generator,
    contract: &FreeformContract,
    prompt: &Prompt,
    model: &str,
    samples: u32,
    jobs: usize,
) -> Result<FreeformOutcome> {
    let k = samples.max(1) as usize;
    let results: Vec<Parsed<JudgeVerdict>> =
        pool::parallel_map(k, jobs, |i| freeform_sample(gen, &prompt.text, i))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
    aggregate_freeform(&results, contract, prompt, model, k as u32)
}

/// Fold the per-sample results (in index order) into one freeform verdict. Mirrors
/// `super::aggregate`: cost/latency/tokens are accounted for *every* call including the dropped
/// samples (the tokens were billed), the mean covers only the samples that parsed, and no parseable
/// sample at all is an error rather than a confident-looking 0.0.
fn aggregate_freeform(
    results: &[Parsed<JudgeVerdict>],
    contract: &FreeformContract,
    prompt: &Prompt,
    model: &str,
    k: u32,
) -> Result<FreeformOutcome> {
    let (mut total_cost, mut any_cost, mut max_latency, mut in_tok, mut out_tok) =
        (0.0_f64, false, 0_u64, 0_u64, 0_u64);
    let mut model_used = model.to_string();
    let (mut parsed, mut parse_failures) = (0_u32, 0_u32);
    let mut first_raw_failure: Option<String> = None;
    let mut first_verdict: Option<JudgeVerdict> = None;
    let mut normals: Vec<f64> = Vec::new();
    let mut injection = prompt.injection_suspected;
    let mut determinism = Determinism::Exact;

    for r in results {
        if let Some(c) = r.cost_usd {
            total_cost += c;
            any_cost = true;
        }
        max_latency = max_latency.max(r.latency_ms);
        in_tok += r.input_tokens;
        out_tok += r.output_tokens;
        if !r.model.is_empty() {
            model_used = r.model.clone();
        }
        injection |= r.injection_suspected;
        // A case is only as reproducible as its least reproducible sample.
        determinism = determinism.weakest(r.determinism);
        match &r.value {
            Some(v) => {
                parsed += 1;
                normals.push(normalized(v));
                if first_verdict.is_none() {
                    first_verdict = Some(v.clone());
                }
            }
            None => {
                parse_failures += 1;
                if first_raw_failure.is_none() {
                    first_raw_failure = r.raw_failure.clone();
                }
            }
        }
    }

    // Not one sample parsed, even after repair: there is no verdict. Surface the raw output instead
    // of recording a 0.0 fail nobody measured — the same refusal `super::aggregate` makes.
    let (verdict, overall) = match (first_verdict, parsed) {
        (Some(v), n) if n > 0 => {
            let mean = normals.iter().sum::<f64>() / normals.len() as f64;
            (v, mean)
        }
        _ => {
            return Err(EngineError::Parse(
                first_raw_failure
                    .map(|raw| {
                        format!("no parseable freeform judge sample; last raw output: {raw}")
                    })
                    .unwrap_or_else(|| "freeform judge produced no parseable samples".to_string()),
            ))
        }
    };

    // Agreement over the samples that actually scored, not the requested count.
    let agreement = if normals.len() > 1 {
        let max = normals.iter().cloned().fold(f64::MIN, f64::max);
        let min = normals.iter().cloned().fold(f64::MAX, f64::min);
        (1.0 - (max - min)).clamp(0.0, 1.0)
    } else {
        1.0
    };

    Ok(FreeformOutcome {
        // The contract's decision, not `verdict.pass`. This single line is the deviation closed.
        pass: overall >= contract.threshold(),
        verdict,
        overall,
        threshold: contract.threshold(),
        contract_id: contract.id().to_string(),
        samples: k,
        samples_parsed: parsed,
        parse_failures,
        agreement,
        cost_usd: if any_cost { Some(total_cost) } else { None },
        model: model_used,
        latency_ms: Some(max_latency),
        input_tokens: Some(in_tok),
        output_tokens: Some(out_tok),
        injection_suspected: injection,
        determinism,
    })
}
