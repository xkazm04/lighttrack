//! LLM-as-judge: provider-agnostic judging built on [`crate::generate`]. The judge is a structured
//! generation — we ask for JSON (schema-enforced) and parse the verdict from the model's text — so
//! any provider judges. Unparseable output triggers one repair re-ask before a sample is dropped.

use std::collections::HashMap;

use serde_json::Value;

use lighttrack_core::{judge_verdict_schema, JudgeVerdict, Rubric};

use crate::claude;
use crate::parse::{extract_json_object, extract_json_value, sample_parsed, Parsed};
use crate::prompts::{build_rubric_prompt, build_rubric_schema};
use crate::providers::generate;
use crate::{
    DimScore, EngineConfig, EngineError, GenOutcome, JudgeOutcome, Result, RubricOutcome, TextOutcome,
};

/// A seam over candidate generation. Production dispatches each judge sample to the configured
/// provider/model; tests drive a deterministic fake. The `index` identifies the sample so a fake can
/// answer deterministically regardless of concurrency; the repair re-ask reuses the same index. This
/// lets the scoring/gating math in [`judge_with`] be unit-tested without burning live API calls.
pub(crate) trait Generator {
    fn generate(&self, index: usize, prompt: &str) -> Result<GenOutcome>;
}

/// The production generator: each call hits the real provider dispatch with the judge schema enforced.
struct ProviderGen<'a> {
    cfg: &'a EngineConfig,
    provider: &'a str,
    model: &'a str,
    schema: Option<Value>,
}

impl Generator for ProviderGen<'_> {
    fn generate(&self, _index: usize, prompt: &str) -> Result<GenOutcome> {
        generate(self.cfg, self.provider, self.model, None, prompt, self.schema.as_ref())
    }
}

/// Parse a `[provider/]model` judge spec into (provider, model). No prefix => anthropic (claude -p).
pub fn parse_judge_spec(spec: &str) -> (String, String) {
    match spec.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.to_string(), m.to_string()),
        _ => ("anthropic".to_string(), spec.to_string()),
    }
}

/// Parse one judge response into `(key, score, reasoning)` for *every* rubric dimension. Scores are
/// clamped to `[0.0, 1.0]`. Returns [`EngineError::Parse`] — carrying the raw output — when the
/// response has no JSON object, or when any dimension's score is absent or non-numeric, so an
/// unparseable verdict is a loud, audited failure rather than a silent all-zero score.
fn parse_sample(raw: &str, rubric: &Rubric) -> Result<Vec<(String, f64, String)>> {
    let out = extract_json_value(raw);
    if out.is_null() {
        return Err(EngineError::Parse(format!(
            "no JSON object in rubric judge output: {raw}"
        )));
    }
    let mut dims = Vec::with_capacity(rubric.dimensions.len());
    for d in &rubric.dimensions {
        let obj = out.get(&d.key);
        let score = obj
            .and_then(|o| o.get("score"))
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                EngineError::Parse(format!(
                    "rubric judge output missing numeric score for dimension '{}': {raw}",
                    d.key
                ))
            })?
            .clamp(0.0, 1.0);
        let reasoning = obj
            .and_then(|o| o.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        dims.push((d.key.clone(), score, reasoning));
    }
    Ok(dims)
}

/// Weighted mean of (dimension, score) pairs using the rubric's weights.
fn weighted(dims: &[(String, f64)], rubric: &Rubric) -> f64 {
    let (mut num, mut den) = (0.0, 0.0);
    for (key, score) in dims {
        let w = rubric
            .dimensions
            .iter()
            .find(|d| &d.key == key)
            .map(|d| d.weight)
            .unwrap_or(1.0);
        num += score * w;
        den += w;
    }
    if den > 0.0 {
        num / den
    } else {
        0.0
    }
}

/// Run the judge on the given provider/model with a fully-formed prompt. The verdict schema is
/// enforced and a single repair re-ask is attempted before an unparseable verdict is a hard error.
pub fn run_judge(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    prompt: &str,
) -> Result<JudgeOutcome> {
    let schema = judge_verdict_schema();
    let parsed = sample_parsed(
        |_i, p| generate(cfg, provider, model, None, p, Some(&schema)),
        0,
        prompt,
        |raw| {
            let json = extract_json_object(raw).ok_or_else(|| {
                EngineError::Parse(format!("no JSON object in judge output: {raw}"))
            })?;
            serde_json::from_str::<JudgeVerdict>(&json)
                .map_err(|e| EngineError::Parse(format!("judge JSON not a verdict: {e}; got: {json}")))
        },
    )?;
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
    })
}

/// Free-form text generation on Claude (anonymization / healing paragraphs).
pub fn run_text(cfg: &EngineConfig, prompt: &str) -> Result<TextOutcome> {
    let (envelope, latency_ms) = claude::invoke(cfg, prompt, &cfg.model, None, None)?;
    Ok(TextOutcome {
        text: envelope
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cost_usd: envelope.get("total_cost_usd").and_then(Value::as_f64),
        model: claude::model_of(&envelope, &cfg.model),
        latency_ms,
    })
}

/// Judge one case against a rubric, averaging over `samples` (self-consistency). Overall + pass are
/// computed here (weighted dimensions + gating floors), never trusted to the model.
#[allow(clippy::too_many_arguments)]
pub fn run_rubric_judge(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    rubric: &Rubric,
    input: &str,
    expected: Option<&str>,
    output: &str,
    samples: u32,
) -> Result<RubricOutcome> {
    let prompt = build_rubric_prompt(rubric, input, expected, output);
    let schema = build_rubric_schema(rubric);
    let gen = ProviderGen { cfg, provider, model, schema: Some(schema) };
    judge_with(&gen, rubric, &prompt, model, samples)
}

/// Core of the rubric judge: drive `samples` generations through `gen` (each with a one-shot repair),
/// then aggregate. Split from [`run_rubric_judge`] so a fake [`Generator`] can exercise the scoring
/// math without live calls. Samples are generated by index and aggregated deterministically in order.
fn judge_with(
    gen: &impl Generator,
    rubric: &Rubric,
    prompt: &str,
    model: &str,
    samples: u32,
) -> Result<RubricOutcome> {
    let k = samples.max(1) as usize;
    let results: Vec<Parsed<Vec<(String, f64, String)>>> = (0..k)
        .map(|i| {
            sample_parsed(
                |idx, p| gen.generate(idx, p),
                i,
                prompt,
                |raw| parse_sample(raw, rubric),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    aggregate(&results, rubric, model, k as u32)
}

/// Fold per-sample [`Parsed`] results (in index order) into per-dimension means, the weighted overall,
/// the floor-gated pass/fail, cross-sample agreement, and honest cost/latency/failure accounting.
fn aggregate(
    results: &[Parsed<Vec<(String, f64, String)>>],
    rubric: &Rubric,
    model: &str,
    k: u32,
) -> Result<RubricOutcome> {
    let mut per_dim: HashMap<String, Vec<f64>> = HashMap::new();
    let mut reasonings: HashMap<String, String> = HashMap::new();
    let mut overalls: Vec<f64> = Vec::new();
    let (mut total_cost, mut any_cost, mut max_latency, mut in_tok, mut out_tok) =
        (0.0_f64, false, 0_u64, 0_u64, 0_u64);
    let mut model_used = model.to_string();
    let mut parse_failures = 0_u32;
    let mut first_raw_failure: Option<String> = None;
    let mut have_reasonings = false;

    for r in results {
        // Account cost/latency/tokens even for a dropped sample — the call still burned real tokens
        // and $, so hiding it would under-report the judge's true expense.
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
        match &r.value {
            Some(dims) => {
                let mut sample: Vec<(String, f64)> = Vec::with_capacity(dims.len());
                for (key, score, reasoning) in dims {
                    per_dim.entry(key.clone()).or_default().push(*score);
                    if !have_reasonings {
                        reasonings.insert(key.clone(), reasoning.clone());
                    }
                    sample.push((key.clone(), *score));
                }
                have_reasonings = true;
                overalls.push(weighted(&sample, rubric));
            }
            None => {
                parse_failures += 1;
                if first_raw_failure.is_none() {
                    first_raw_failure = r.raw_failure.clone();
                }
            }
        }
    }

    // No sample parsed (even after repair): there is no real score to report. Surface the raw output
    // instead of recording a confident-looking 0.0 fail.
    if overalls.is_empty() {
        return Err(EngineError::Parse(
            first_raw_failure
                .map(|raw| format!("no parseable rubric judge sample; last raw output: {raw}"))
                .unwrap_or_else(|| "rubric judge produced no parseable samples".to_string()),
        ));
    }

    let dimensions: Vec<DimScore> = rubric
        .dimensions
        .iter()
        .map(|d| {
            let v = per_dim.get(&d.key).cloned().unwrap_or_default();
            let mean = if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            };
            DimScore {
                key: d.key.clone(),
                score: mean,
                reasoning: reasonings.get(&d.key).cloned().unwrap_or_default(),
                weight: d.weight,
            }
        })
        .collect();

    let overall = {
        let den: f64 = dimensions.iter().map(|d| d.weight).sum();
        if den > 0.0 {
            dimensions.iter().map(|d| d.score * d.weight).sum::<f64>() / den
        } else {
            0.0
        }
    };
    let pass = overall >= rubric.threshold
        && rubric.dimensions.iter().all(|d| {
            let s = dimensions
                .iter()
                .find(|x| x.key == d.key)
                .map(|x| x.score)
                .unwrap_or(0.0);
            d.floor.is_none_or(|f| s >= f)
        });
    // Agreement is measured over the samples that actually scored, not the requested count — a lone
    // surviving sample has nothing to disagree with, so it reports full agreement.
    let agreement = if overalls.len() > 1 {
        let max = overalls.iter().cloned().fold(f64::MIN, f64::max);
        let min = overalls.iter().cloned().fold(f64::MAX, f64::min);
        (1.0 - (max - min)).clamp(0.0, 1.0)
    } else {
        1.0
    };

    Ok(RubricOutcome {
        dimensions,
        overall,
        pass,
        cost_usd: if any_cost { Some(total_cost) } else { None },
        latency_ms: Some(max_latency),
        tokens: Some(in_tok + out_tok),
        input_tokens: Some(in_tok),
        output_tokens: Some(out_tok),
        model: model_used,
        samples: k,
        agreement,
        parse_failures,
    })
}

#[cfg(test)]
mod tests;
