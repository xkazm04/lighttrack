//! LLM-as-judge: provider-agnostic judging built on [`crate::generate`]. The judge is a structured
//! generation — we ask for JSON (schema-enforced) and parse the verdict from the model's text — so
//! any provider judges. Unparseable output triggers one repair re-ask before a sample is dropped.

use std::collections::HashMap;

use serde_json::Value;

use lighttrack_core::{judge_verdict_schema, JudgeVerdict, Rubric};

use crate::invocation::{self, Invocation};
use crate::parse::{extract_json_object, extract_json_value, sample_parsed, Parsed};
use crate::pool;
use crate::prompts::{build_rubric_prompt, build_rubric_schema, Prompt};
use crate::scorers::{self, DetScore};
use crate::{
    Determinism, DimScore, EngineConfig, EngineError, GenOutcome, JudgeOutcome, Result,
    RubricOutcome, TextOutcome,
};
use grounding::Grounding;

/// A seam over candidate generation. Production dispatches each judge sample to the configured
/// provider/model; tests drive a deterministic fake. The `index` identifies the sample so a fake can
/// answer deterministically regardless of concurrency; the repair re-ask reuses the same index. This
/// lets the scoring/gating math in [`judge_with`] be unit-tested without burning live API calls. The
/// `Sync` bound lets samples be judged concurrently (bounded by `jobs`) across scoped threads.
pub(crate) trait Generator: Sync {
    fn generate(&self, index: usize, prompt: &str) -> Result<GenOutcome>;
}

/// The production generator: each call hits the real provider dispatch with the judge schema enforced.
struct ProviderGen<'a> {
    cfg: &'a EngineConfig,
    provider: &'a str,
    model: &'a str,
    schema: Option<Value>,
    /// More than one sample was asked for: draw each one unpinned and stamp it `sampled`.
    sampled: bool,
}

/// Build the production generator for a schema-enforced judge call. Shared with [`batch`], whose
/// calls must reach the provider through exactly this path — same determinism request, same dispatch.
pub(crate) fn provider_gen<'a>(
    cfg: &'a EngineConfig,
    provider: &'a str,
    model: &'a str,
    schema: Value,
    samples: u32,
) -> impl Generator + 'a {
    ProviderGen {
        cfg,
        provider,
        model,
        schema: Some(schema),
        sampled: samples > 1,
    }
}

impl Generator for ProviderGen<'_> {
    fn generate(&self, _index: usize, prompt: &str) -> Result<GenOutcome> {
        // Self-consistency is a distribution, and a pinned request has none: N draws at temperature
        // 0 and one seed over one prompt are one draw billed N times, so `agreement` would read 1.0
        // however ambiguous the case. Several samples are therefore drawn unpinned and say so —
        // the same rule `--gen-samples > 1` follows on the generation half.
        if self.sampled {
            let mut out = crate::providers::generate(
                self.cfg,
                self.provider,
                self.model,
                None,
                prompt,
                self.schema.as_ref(),
            )?;
            out.determinism = Determinism::Sampled;
            return Ok(out);
        }
        // A single verdict is a measurement: request deterministic sampling (temperature 0 + fixed
        // seed where the provider takes one), so re-running an eval reproduces its score.
        crate::providers::generate_deterministic(
            self.cfg,
            self.provider,
            self.model,
            None,
            prompt,
            self.schema.as_ref(),
        )
    }
}

/// One rubric sample's parsed dimensions: `(key, clamped score, reasoning)` for every dimension.
type SampleDims = Vec<(String, f64, String)>;

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
fn parse_sample(raw: &str, rubric: &Rubric) -> Result<SampleDims> {
    let out = extract_json_value(raw);
    if out.is_null() {
        return Err(EngineError::Parse(format!(
            "no JSON object in rubric judge output: {raw}"
        )));
    }
    parse_sample_value(&out, rubric)
}

/// Read one already-extracted verdict object into per-dimension scores. Split from [`parse_sample`]
/// so a batched response's entries — which arrive as elements of an array, not as whole documents —
/// are read by exactly the same rules as a single verdict.
pub(crate) fn parse_sample_value(out: &Value, rubric: &Rubric) -> Result<SampleDims> {
    let raw = out;
    let mut dims = Vec::with_capacity(rubric.dimensions.len());
    // Only the dimensions the model was actually asked about; deterministic ones are scored locally.
    for d in rubric.dimensions.iter().filter(|d| d.kind.is_llm()) {
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
    prompt: &Prompt,
) -> Result<JudgeOutcome> {
    let schema = judge_verdict_schema();
    let parsed = sample_parsed(
        // Deterministic sampling — a verdict is a measurement (see ProviderGen::generate).
        |_i, p| {
            crate::providers::generate_deterministic(cfg, provider, model, None, p, Some(&schema))
        },
        0,
        &prompt.text,
        |raw| {
            let json = extract_json_object(raw).ok_or_else(|| {
                EngineError::Parse(format!("no JSON object in judge output: {raw}"))
            })?;
            serde_json::from_str::<JudgeVerdict>(&json).map_err(|e| {
                EngineError::Parse(format!("judge JSON not a verdict: {e}; got: {json}"))
            })
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
        injection_suspected: prompt.injection_suspected || parsed.injection_suspected,
        determinism: parsed.determinism,
    })
}

/// Free-form text generation on Claude (anonymization / healing paragraphs).
pub fn run_text(cfg: &EngineConfig, prompt: &str) -> Result<TextOutcome> {
    let out = invocation::run(
        &cfg.claude(),
        &Invocation::generate(prompt, &cfg.model).with_bare(cfg.bare),
    )?;
    Ok(TextOutcome {
        text: out.text,
        cost_usd: out.cost_usd,
        model: out.model,
        latency_ms: out.latency_ms,
    })
}

/// Judge one case against a rubric, averaging over `samples` (self-consistency). Overall + pass are
/// computed here (weighted dimensions + gating floors), never trusted to the model. `jobs` bounds how
/// many of the `samples` are generated concurrently (`1` = fully sequential).
///
/// A rubric may mix LLM-judged and deterministic dimensions (see [`crate::scorers`]). The
/// deterministic ones are checked locally, once, at zero tokens and zero cost, and are neither shown
/// to the model nor requested from it; they then flow through weighting, floors and the overall
/// exactly like an LLM dimension. An all-deterministic rubric makes **no provider call at all**.
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
    jobs: usize,
) -> Result<RubricOutcome> {
    // The sandbox is run-scoped config, so an `exec` rubric works through the ordinary entry point
    // once the operator has configured one; without it, `evaluate_all` refuses by name.
    let sandbox = cfg
        .sandbox
        .as_deref()
        .map(|r| r as &dyn crate::sandbox::SandboxRunner);
    run_rubric_judge_sandboxed(
        cfg, provider, model, rubric, input, expected, output, samples, jobs, sandbox,
    )
}

/// [`run_rubric_judge`] with a sandbox available, so a rubric may carry `exec` dimensions.
///
/// Additive rather than a new parameter on the original: every existing caller judges rubrics that
/// cannot need a sandbox, and `exec` is refused loudly when one is absent rather than skipped.
/// Callers should run [`crate::sandbox::preflight`] **once per run** before the first case.
#[allow(clippy::too_many_arguments)]
pub fn run_rubric_judge_sandboxed(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    rubric: &Rubric,
    input: &str,
    expected: Option<&str>,
    output: &str,
    samples: u32,
    jobs: usize,
    exec: Option<&dyn crate::sandbox::SandboxRunner>,
) -> Result<RubricOutcome> {
    let case = Case {
        input,
        expected,
        output,
        evidence: None,
    };
    judge_case(cfg, provider, model, rubric, &case, samples, jobs, exec)
}

/// [`run_rubric_judge`] for a case that carries **evidence passages** (what the system under test
/// was given to answer from), so the rubric may carry a `grounding` dimension.
///
/// Additive, like [`run_rubric_judge_sandboxed`]: no existing caller has evidence, and a `grounding`
/// dimension judged through any other entry point is refused by name rather than scored. Each passage
/// is fenced separately; an empty slice is legitimate evidence (retrieval found nothing), under which
/// every claim is unsupported.
#[allow(clippy::too_many_arguments)]
pub fn run_rubric_judge_with_evidence(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    rubric: &Rubric,
    input: &str,
    expected: Option<&str>,
    output: &str,
    evidence: &[String],
    samples: u32,
    jobs: usize,
) -> Result<RubricOutcome> {
    let sandbox = cfg
        .sandbox
        .as_deref()
        .map(|r| r as &dyn crate::sandbox::SandboxRunner);
    let case = Case {
        input,
        expected,
        output,
        evidence: Some(evidence),
    };
    judge_case(cfg, provider, model, rubric, &case, samples, jobs, sandbox)
}

/// One judged case's text.
struct Case<'a> {
    input: &'a str,
    expected: Option<&'a str>,
    output: &'a str,
    evidence: Option<&'a [String]>,
}

#[allow(clippy::too_many_arguments)]
fn judge_case(
    cfg: &EngineConfig,
    provider: &str,
    model: &str,
    rubric: &Rubric,
    case: &Case<'_>,
    samples: u32,
    jobs: usize,
    exec: Option<&dyn crate::sandbox::SandboxRunner>,
) -> Result<RubricOutcome> {
    // Refused before the sandbox or any model spends anything.
    grounding::grounding_dim(rubric, case.evidence.is_some())?;
    let det = scorers::evaluate_all(rubric, case.expected, case.output, exec)?;
    // The prompt is built even for an all-deterministic rubric: it is where the fence inspects the
    // candidate, so "this content tried to imitate a judge boundary" stays a reportable fact whether
    // or not a model ends up seeing it. `judge_grounded_with` decides whether to send it.
    let prompt = build_rubric_prompt(rubric, case.input, case.expected, case.output);
    let gen = provider_gen(cfg, provider, model, build_rubric_schema(rubric), samples);
    let decompose = provider_gen(
        cfg,
        provider,
        model,
        crate::prompts::grounding::decompose_schema(),
        samples,
    );
    let verify = provider_gen(
        cfg,
        provider,
        model,
        crate::prompts::grounding::verify_schema(),
        samples,
    );
    let g = case.evidence.map(|evidence| Grounding {
        output: case.output,
        evidence,
        decompose: &decompose,
        verify: &verify,
    });
    judge_grounded_with(
        &gen,
        rubric,
        &prompt,
        model,
        samples,
        jobs,
        &det,
        g.as_ref(),
    )
}

/// Fold every sample's fence signal (repair re-asks can raise their own) with the prompt's.
fn any_injection(prompt: &Prompt, results: &[Parsed<SampleDims>]) -> bool {
    prompt.injection_suspected || results.iter().any(|r| r.injection_suspected)
}

/// Core of the rubric judge: drive `samples` generations through `gen` (each with a one-shot repair),
/// then aggregate together with the already-computed deterministic dimensions `det`. Split from
/// [`run_rubric_judge`] so a fake [`Generator`] can exercise the scoring math without live calls.
/// Samples are generated by index — up to `jobs` concurrently — and aggregated deterministically in
/// index order, so `jobs == 1` and `jobs == k` are byte-identical. A rubric with no LLM dimension
/// runs `k = 0` samples: the generator is never called, and no tokens are spent.
#[cfg(test)]
fn judge_with(
    gen: &impl Generator,
    rubric: &Rubric,
    prompt: &Prompt,
    model: &str,
    samples: u32,
    jobs: usize,
    det: &[DetScore],
) -> Result<RubricOutcome> {
    judge_grounded_with(gen, rubric, prompt, model, samples, jobs, det, None)
}

/// [`judge_with`] for a case that may carry evidence. A `grounding` dimension without it is refused.
/// Its decompose/verify calls fold into each sample, so it is sampled, weighted, floored and spread
/// into `agreement` exactly like an `llm` dimension.
#[allow(clippy::too_many_arguments)]
fn judge_grounded_with(
    gen: &impl Generator,
    rubric: &Rubric,
    prompt: &Prompt,
    model: &str,
    samples: u32,
    jobs: usize,
    det: &[DetScore],
    grounding: Option<&Grounding<'_>>,
) -> Result<RubricOutcome> {
    let gdim = grounding::grounding_dim(rubric, grounding.is_some())?;
    let k = if scorers::has_llm_dims(rubric) || gdim.is_some() {
        samples.max(1) as usize
    } else {
        0
    };
    let results: Vec<Result<_>> = pool::parallel_map(k, jobs, |i| {
        grounding::sample(gen, rubric, &prompt.text, i, gdim.zip(grounding))
    });
    let (results, grounds): (Vec<Parsed<SampleDims>>, Vec<_>) = results
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    let injection_suspected = any_injection(prompt, &results);
    // A case is only as reproducible as its least reproducible sample.
    let determinism = results
        .iter()
        .fold(Determinism::Exact, |acc, r| acc.weakest(r.determinism));
    let mut outcome = aggregate(&results, rubric, model, k as u32, det)?;
    if let Some(d) = gdim {
        grounding::stamp(&mut outcome, d, &grounds);
    }
    outcome.injection_suspected = injection_suspected;
    outcome.determinism = determinism;
    Ok(outcome)
}

/// Fold per-sample [`Parsed`] results (in index order) plus the deterministic dimensions `det` into
/// per-dimension means, the weighted overall, the floor-gated pass/fail, cross-sample agreement, and
/// honest cost/latency/failure accounting.
///
/// **Agreement covers the LLM dimensions only.** A deterministic dimension is scored once and is
/// exactly reproducible, so folding it in would drag every rubric's agreement toward 1.0 and hide the
/// judge's real disagreement. `overalls` are therefore the per-sample weighted means over the sampled
/// (LLM) dimensions, while the outcome's `overall` is the full weighted mean over every dimension.
fn aggregate(
    results: &[Parsed<SampleDims>],
    rubric: &Rubric,
    model: &str,
    k: u32,
    det: &[DetScore],
) -> Result<RubricOutcome> {
    let mut per_dim: HashMap<String, Vec<f64>> = HashMap::new();
    // Every sample's reasoning, in index order — not just the first. Samples 2..k were billed; their
    // justification is the audit trail for the mean they moved.
    let mut reasonings: HashMap<String, Vec<String>> = HashMap::new();
    let mut overalls: Vec<f64> = Vec::new();
    let (mut total_cost, mut any_cost, mut max_latency, mut in_tok, mut out_tok) =
        (0.0_f64, false, 0_u64, 0_u64, 0_u64);
    // With no LLM dimension no model ran, so claiming one scored the case would be a lie.
    let mut model_used = if k == 0 {
        "deterministic".into()
    } else {
        model.to_string()
    };
    let (mut parse_failures, mut parsed) = (0_u32, 0_u32);
    let mut first_raw_failure: Option<String> = None;

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
                parsed += 1;
                let mut sample: Vec<(String, f64)> = Vec::with_capacity(dims.len());
                for (key, score, reasoning) in dims {
                    per_dim.entry(key.clone()).or_default().push(*score);
                    if !reasoning.is_empty() {
                        reasonings
                            .entry(key.clone())
                            .or_default()
                            .push(reasoning.clone());
                    }
                    sample.push((key.clone(), *score));
                }
                // A sample that scored no dimension (a grounding-only rubric whose output yielded
                // zero claims) has no overall to disagree about.
                if !sample.is_empty() {
                    overalls.push(weighted(&sample, rubric));
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

    // No sample parsed (even after repair): there is no real score to report. Surface the raw output
    // instead of recording a confident-looking 0.0 fail. (`k == 0` is the all-deterministic rubric,
    // which asked for no samples and is scored entirely from `det`.)
    if k > 0 && parsed == 0 {
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
            // A deterministic dimension carries its own single verdict + reason; an LLM dimension is
            // the mean over the samples that parsed. Both land in the same weighting/floor math.
            let local = det.iter().find(|s| s.key == d.key);
            let v = per_dim.get(&d.key).cloned().unwrap_or_default();
            // A voided dimension (an `exec` whose sandbox was unavailable) reports 0.0 as a
            // placeholder and is excluded from the math below by `voided`, never by its score.
            let (mean, voided) = match local {
                Some(s) => match s.score {
                    Some(x) => (x, false),
                    None => (0.0, true),
                },
                // A grounding dimension no sample scored: every parsed cut yielded zero claims.
                None if v.is_empty() => (0.0, d.kind.is_grounding()),
                None => (v.iter().sum::<f64>() / v.len() as f64, false),
            };
            DimScore {
                key: d.key.clone(),
                score: mean,
                reasonings: match local {
                    Some(s) => vec![s.reasoning.clone()],
                    None => reasonings.get(&d.key).cloned().unwrap_or_default(),
                },
                weight: d.weight,
                floor: d.floor,
                // A measurement that did not happen cannot breach a floor.
                floor_hit: !voided && d.floor.is_some_and(|f| mean < f),
                voided,
                grounding: None,
            }
        })
        .collect();

    // Every dimension voided means the sandbox was down for this case, not that the candidate was
    // bad. There is no verdict to report, and reporting 0.0/fail would be a confident-looking lie —
    // the same refusal as the no-parseable-sample path above.
    if !dimensions.is_empty() && dimensions.iter().all(|d| d.voided) {
        let why: Vec<String> = rubric
            .dimensions
            .iter()
            .map(|d| match d.kind.is_grounding() {
                true => format!("'{}': the output yielded zero claims", d.key),
                false => format!("'{}': the sandbox was unavailable", d.key),
            })
            .collect();
        return Err(EngineError::Other(format!(
            "every rubric dimension was voided, so this case has no verdict ({})",
            why.join("; ")
        )));
    }

    let overall = {
        // Voided dimensions leave the denominator as well as the numerator, so the dimensions that
        // *were* measured keep their relative weights instead of being quietly re-based.
        let den: f64 = dimensions
            .iter()
            .filter(|d| !d.voided)
            .map(|d| d.weight)
            .sum();
        if den > 0.0 {
            dimensions
                .iter()
                .filter(|d| !d.voided)
                .map(|d| d.score * d.weight)
                .sum::<f64>()
                / den
        } else {
            0.0
        }
    };
    // Identical gating to before, now read off the per-dimension `floor_hit` the outcome carries.
    let pass = overall >= rubric.threshold && dimensions.iter().all(|d| !d.floor_hit);
    // Agreement is measured over the samples that actually scored, not the requested count — a lone
    // surviving sample has nothing to disagree with, so it reports full agreement. An
    // all-deterministic rubric took no samples at all and is, by construction, in full agreement.
    let agreement = if overalls.len() > 1 {
        let max = overalls.iter().cloned().fold(f64::MIN, f64::max);
        let min = overalls.iter().cloned().fold(f64::MAX, f64::min);
        (1.0 - (max - min)).clamp(0.0, 1.0)
    } else {
        1.0
    };

    Ok(RubricOutcome {
        // Judged alone unless a batched caller stamps its size afterwards — aggregate itself is
        // batch-agnostic on purpose, so no verdict is scored by different code for having shared a call.
        batch_size: None,
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
        samples_parsed: parsed,
        agreement,
        parse_failures,
        // Both set by judge_with, which owns the prompt's fence signal and the per-sample stamps.
        injection_suspected: false,
        determinism: Determinism::BestEffort,
    })
}

pub(crate) mod batch;
pub(crate) mod grounding;

#[cfg(test)]
mod tests;
