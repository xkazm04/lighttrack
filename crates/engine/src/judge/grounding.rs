//! The `grounding` dimension kind: decompose the output into standalone claims, verify each claim
//! against the case's evidence passages, and score supported over claims **issued**.
//!
//! Both steps are judge-model calls through the ordinary [`Generator`] seam, so a sample of a rubric
//! with a grounding dimension is up to three calls: the rubric prompt (only when the rubric also has
//! `llm` dimensions), the cut, and the check. They fold into one sample, so the grounding score
//! lands in the same per-sample overall that `agreement` spreads over, and in the same weighting and
//! floors as an `llm` dimension.
//!
//! The cutter is part of the instrument: its prompts live in [`crate::prompts::grounding`], whose
//! pin every grounding verdict carries.

use serde_json::Value;

use lighttrack_core::{ClaimVerdict, GroundingDetail, Rubric, RubricDimension};

use super::{parse_sample, Generator, SampleDims};
use crate::parse::{extract_json_value, sample_parsed, Parsed};
use crate::prompts::grounding::{decompose_prompt, instrument_pin, verify_prompt};
use crate::{EngineError, Result, RubricOutcome};

/// A case's evidence plus the generators that cut and check against it.
pub(crate) struct Grounding<'a> {
    pub(crate) output: &'a str,
    pub(crate) evidence: &'a [String],
    pub(crate) decompose: &'a dyn Generator,
    pub(crate) verify: &'a dyn Generator,
}

/// One sample's grounding result. `score: None` = the output yielded zero claims.
pub(crate) struct GroundSample {
    score: Option<f64>,
    reasoning: String,
    verdicts: Vec<ClaimVerdict>,
    missing: u32,
    stray: u32,
}

/// The rubric's grounding dimension, if any — refused by name when this case cannot score it.
pub(crate) fn grounding_dim(
    rubric: &Rubric,
    has_evidence: bool,
) -> Result<Option<&RubricDimension>> {
    let mut dims = rubric.dimensions.iter().filter(|d| d.kind.is_grounding());
    let Some(d) = dims.next() else {
        return Ok(None);
    };
    if let Some(second) = dims.next() {
        return Err(EngineError::Other(format!(
            "rubric dimensions '{}' and '{}' are both `grounding`; the procedure takes no \
             per-dimension setting, so a second one would bill the same verdict twice",
            d.key, second.key
        )));
    }
    if !has_evidence {
        return Err(EngineError::Other(format!(
            "rubric dimension '{}' is `grounding` but this case carries no evidence passages \
             (judge it through run_rubric_judge_with_evidence; batched judging carries none)",
            d.key
        )));
    }
    Ok(Some(d))
}

fn parse_claims(raw: &str) -> Result<Vec<String>> {
    let v = extract_json_value(raw);
    let arr = v["claims"].as_array().ok_or_else(|| {
        EngineError::Parse(format!(
            "grounding decomposition has no `claims` array: {raw}"
        ))
    })?;
    Ok(arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect())
}

/// `(claim number, supported, reason)` per entry; a malformed entry keeps `None`s and is judged stray.
type RawVerdict = (Option<u64>, Option<bool>, String);

fn parse_verdicts(raw: &str) -> Result<Vec<RawVerdict>> {
    let v = extract_json_value(raw);
    let arr = v["verdicts"].as_array().ok_or_else(|| {
        EngineError::Parse(format!(
            "grounding verification has no `verdicts` array: {raw}"
        ))
    })?;
    Ok(arr
        .iter()
        .map(|e| {
            let reason = e["reason"].as_str().unwrap_or("").to_string();
            (e["claim"].as_u64(), e["supported"].as_bool(), reason)
        })
        .collect())
}

/// Match verdicts to claims by number. The denominator is the claims issued: a claim with no verdict
/// is unsupported, and a verdict for a number nobody issued (or a second one for the same claim) is
/// ignored. Both are counted, so a shrinking denominator cannot hide.
fn score_claims(sample: usize, claims: &[String], raw: Vec<RawVerdict>) -> GroundSample {
    let n = claims.len();
    let mut got: Vec<Option<(bool, String)>> = vec![None; n];
    let mut stray = 0;
    for (num, supported, reason) in raw {
        match (num, supported) {
            (Some(k), Some(s)) if k >= 1 && (k as usize) <= n && got[k as usize - 1].is_none() => {
                got[k as usize - 1] = Some((s, reason))
            }
            _ => stray += 1,
        }
    }
    let verdicts: Vec<ClaimVerdict> = claims
        .iter()
        .zip(got)
        .enumerate()
        .map(|(i, (claim, v))| ClaimVerdict {
            sample: sample as u32,
            number: i as u32 + 1,
            claim: claim.clone(),
            missing: v.is_none(),
            supported: v.as_ref().is_some_and(|(s, _)| *s),
            reason: v.map(|(_, r)| r).unwrap_or_default(),
        })
        .collect();
    let supported = verdicts.iter().filter(|v| v.supported).count();
    let missing = verdicts.iter().filter(|v| v.missing).count() as u32;
    let mut reasoning = format!("grounding: {supported}/{n} claims supported by the evidence");
    if missing + stray > 0 {
        reasoning.push_str(&format!(
            " ({missing} without a verdict, counted unsupported; {stray} stray verdict(s) ignored)"
        ));
    }
    GroundSample {
        score: Some(supported as f64 / n as f64),
        reasoning,
        verdicts,
        missing,
        stray,
    }
}

/// Fold one step's accounting into a sample. The steps run one after another, so latency adds.
fn absorb<T, U>(into: &mut Parsed<T>, from: Parsed<U>) {
    if let Some(c) = from.cost_usd {
        into.cost_usd = Some(into.cost_usd.unwrap_or(0.0) + c);
    }
    into.latency_ms += from.latency_ms;
    into.input_tokens += from.input_tokens;
    into.output_tokens += from.output_tokens;
    into.determinism = into.determinism.weakest(from.determinism);
    into.injection_suspected |= from.injection_suspected;
    if !from.model.is_empty() {
        into.model = from.model;
    }
    if into.raw_failure.is_none() {
        into.raw_failure = from.raw_failure;
    }
}

/// Cut, then check. `value: None` = a step stayed unparseable after its repair re-ask.
fn ground(g: &Grounding<'_>, index: usize) -> Result<Parsed<GroundSample>> {
    let dp = decompose_prompt(g.output);
    let gen = |i: usize, p: &str| g.decompose.generate(i, p);
    let mut cut = sample_parsed(gen, index, &dp.text, parse_claims)?;
    cut.injection_suspected |= dp.injection_suspected;
    let claims = cut.value.take();
    let mut out = Parsed::empty();
    absorb(&mut out, cut);
    let Some(claims) = claims else {
        return Ok(out);
    };
    if claims.is_empty() {
        out.value = Some(GroundSample {
            score: None,
            reasoning: String::new(),
            verdicts: Vec::new(),
            missing: 0,
            stray: 0,
        });
        return Ok(out);
    }
    let vp = verify_prompt(g.evidence, &claims);
    let gen = |i: usize, p: &str| g.verify.generate(i, p);
    let mut checked = sample_parsed(gen, index, &vp.text, parse_verdicts)?;
    checked.injection_suspected |= vp.injection_suspected;
    let raw = checked.value.take();
    absorb(&mut out, checked);
    out.value = raw.map(|r| score_claims(index, &claims, r));
    Ok(out)
}

/// One sample of a rubric that may carry a grounding dimension: the rubric-prompt call when there
/// are `llm` dimensions, then the cut and check, folded into a single [`SampleDims`]. A sample is
/// dropped whole when any of its steps stayed unparseable, exactly like a bad rubric verdict.
pub(crate) fn sample(
    gen: &impl Generator,
    rubric: &Rubric,
    prompt: &str,
    index: usize,
    grounding: Option<(&RubricDimension, &Grounding<'_>)>,
) -> Result<(Parsed<SampleDims>, Option<GroundSample>)> {
    let mut s = if crate::scorers::has_llm_dims(rubric) {
        let call = |i: usize, p: &str| gen.generate(i, p);
        sample_parsed(call, index, prompt, |raw| parse_sample(raw, rubric))?
    } else {
        let mut empty = Parsed::empty();
        empty.value = Some(Vec::new());
        empty
    };
    let Some((dim, g)) = grounding else {
        return Ok((s, None));
    };
    let mut step = ground(g, index)?;
    let gs = step.value.take();
    absorb(&mut s, step);
    let (Some(dims), Some(gs)) = (s.value.as_mut(), gs) else {
        s.value = None;
        return Ok((s, None));
    };
    // Zero claims: the dimension is absent from this sample, never scored 1.0 or 0.0.
    if let Some(score) = gs.score {
        dims.push((dim.key.clone(), score, gs.reasoning.clone()));
    }
    Ok((s, Some(gs)))
}

/// Attach the per-claim verdicts, the counters and the instrument pin to the grounding dimension.
pub(crate) fn stamp(
    out: &mut RubricOutcome,
    dim: &RubricDimension,
    grounds: &[Option<GroundSample>],
) {
    let scored: Vec<&GroundSample> = grounds.iter().flatten().collect();
    let detail = GroundingDetail {
        version: instrument_pin(),
        claims: scored
            .iter()
            .flat_map(|g| g.verdicts.iter().cloned())
            .collect(),
        claims_issued: scored.iter().map(|g| g.verdicts.len() as u32).sum(),
        missing_verdicts: scored.iter().map(|g| g.missing).sum(),
        stray_verdicts: scored.iter().map(|g| g.stray).sum(),
        unscored_samples: scored.iter().filter(|g| g.score.is_none()).count() as u32,
    }
    .capped();
    if let Some(d) = out.dimensions.iter_mut().find(|d| d.key == dim.key) {
        if d.voided {
            d.reasonings
                .push("grounding: the output yielded zero claims → unscored".to_string());
        }
        d.grounding = Some(detail);
    }
}

#[cfg(test)]
mod tests;
