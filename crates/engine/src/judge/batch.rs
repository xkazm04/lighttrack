//! Batched rubric judging: N cases judged in one provider call instead of N.
//!
//! **Why.** A judge verdict is ~200 tokens of judgement carried on top of a full provider context —
//! measured here at ~55k cached + 4k created tokens per `claude -p` invocation, i.e. the overhead is
//! two orders of magnitude larger than the payload. Judging is `cases × samples` invocations, so the
//! overhead is what a benchmark actually spends. Batching amortizes it across a batch; on a
//! subscription that is wall-clock and rate-limit headroom rather than money.
//!
//! **What it deliberately does not change.** Batching is a *transport* change. A batched response is
//! split back into one [`Parsed<SampleDims>`] per case, and the scoring math — weights, floors,
//! agreement over LLM dimensions only, determinism folding — runs in [`super::aggregate`] exactly as
//! it does for a single case. No verdict is scored by different code because it arrived in a batch.
//!
//! **What it does change, and is recorded.** A judge that sees ten cases at once may anchor on them,
//! so a batched score is not interchangeable with an unbatched one. Every outcome therefore carries
//! `batch_size`, and its cost is marked amortized rather than measured. Comparing a batched run to an
//! unbatched baseline is a methodology change, not a quality change, and the provenance is there so a
//! consumer can refuse the comparison.

use std::collections::HashMap;

use serde_json::Value;

use lighttrack_core::Rubric;

use crate::parse::{extract_json_value, sample_parsed, Parsed};
use crate::pool;
use crate::prompts::{build_batch_rubric_prompt, BatchEntry, CASE_ID};
use crate::scorers;
use crate::{Determinism, EngineError, Result, RubricOutcome};

use super::{aggregate, Generator, SampleDims};

/// One case to judge. Borrowed: a batch is assembled per call and never outlives its cases.
pub struct BatchCase<'a> {
    pub input: &'a str,
    pub expected: Option<&'a str>,
    pub output: &'a str,
}

/// A batched sample's verdicts, keyed by the case id the model echoed.
type BatchDims = HashMap<String, SampleDims>;

/// Cases are addressed by their index in the caller's slice, so an id is stable across samples even
/// though presentation order rotates.
fn case_id(index: usize) -> String {
    format!("case-{index}")
}

/// Split a batched response into per-case dimensions, matching on the echoed id.
///
/// Position is never trusted. A response missing a case, repeating one, or inventing one yields a map
/// that simply lacks (or ignores) those ids; the caller turns a missing id into *that case's* parse
/// failure. This is the whole safety argument for batching: the failure mode of zipping by position
/// is a silent one-case shift that misattributes every subsequent verdict, which no downstream check
/// would ever catch.
fn parse_batch(raw: &str, rubric: &Rubric, want: &[String]) -> Result<BatchDims> {
    let root = extract_json_value(raw);
    let arr = root
        .get("verdicts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            EngineError::Parse(format!(
                "batched judge output has no `verdicts` array: {raw}"
            ))
        })?;
    let mut out = BatchDims::new();
    for v in arr {
        let Some(id) = v.get(CASE_ID).and_then(Value::as_str) else {
            continue; // an entry that will not say what it answers cannot be attributed to anything
        };
        if !want.iter().any(|w| w == id) || out.contains_key(id) {
            continue; // unknown or duplicated id: drop it rather than let it claim a case
        }
        // Reuse the single-case parser so a batched verdict and a lone one are read identically.
        if let Ok(dims) = super::parse_sample_value(v, rubric) {
            out.insert(id.to_string(), dims);
        }
    }
    if out.is_empty() {
        return Err(EngineError::Parse(format!(
            "batched judge output attributed no verdict to any of the {} cases: {raw}",
            want.len()
        )));
    }
    Ok(out)
}

/// Present `n` cases in an order rotated by `sample`, so a case does not sit in the same position in
/// every sample. Position effects then surface as cross-sample disagreement — which `agreement`
/// already measures — instead of biasing every sample the same way and looking like consensus.
fn rotated(n: usize, sample: usize) -> Vec<usize> {
    (0..n).map(|i| (i + sample) % n).collect()
}

/// Judge `cases` against `rubric` in batches, returning one outcome per case in input order.
///
/// A case that never received a parseable verdict from any sample fails on its own, so one bad case
/// costs one case. An all-deterministic rubric makes no provider call at all, exactly as it does when
/// judging singly.
///
/// The outer `Result` is the *batch's* fate — a provider/transport error, or a rubric so malformed
/// that no case can be scored. The inner one is a single case's: a verdict the model never
/// attributed to it, or its own deterministic check failing. One bad case costs one case; one dead
/// call costs the batch, and the caller may retry it unbatched.
#[allow(clippy::too_many_arguments)]
pub fn run_rubric_batch(
    cfg: &crate::EngineConfig,
    provider: &str,
    model: &str,
    rubric: &Rubric,
    cases: &[BatchCase<'_>],
    samples: u32,
    jobs: usize,
) -> Result<Vec<Result<RubricOutcome>>> {
    let schema = crate::prompts::build_batch_rubric_schema(rubric);
    let gen = super::provider_gen(cfg, provider, model, schema);
    batch_with(&gen, rubric, cases, model, samples, jobs)
}

/// Core of batched judging, split from [`run_rubric_batch`] so a fake [`Generator`] can exercise the
/// split-and-aggregate path — including partial and misattributed responses — without live calls.
pub(crate) fn batch_with(
    gen: &impl Generator,
    rubric: &Rubric,
    cases: &[BatchCase<'_>],
    model: &str,
    samples: u32,
    jobs: usize,
) -> Result<Vec<Result<RubricOutcome>>> {
    let n = cases.len();
    let ids: Vec<String> = (0..n).map(case_id).collect();

    // Deterministic dimensions are local and free, and are evaluated for every case BEFORE any call
    // is made: a rubric with a `regex` and no pattern is an operator error that must cost nothing,
    // and it is a fact about the rubric, so it fails the batch loudly rather than one case quietly.
    let det: Vec<Vec<scorers::DetScore>> = cases
        .iter()
        .map(|c| scorers::evaluate_all(rubric, c.expected, c.output))
        .collect::<Result<Vec<_>>>()?;

    let k = if scorers::has_llm_dims(rubric) {
        samples.max(1) as usize
    } else {
        0
    };

    // Each sample judges the whole batch once, with the cases rotated.
    let batched: Vec<Result<Parsed<BatchDims>>> = pool::parallel_map(k, jobs, |s| {
        let order = rotated(n, s);
        let entries: Vec<BatchEntry<'_>> = order
            .iter()
            .map(|&i| BatchEntry {
                id: ids[i].clone(),
                input: cases[i].input,
                expected: cases[i].expected,
                output: cases[i].output,
            })
            .collect();
        let prompt = build_batch_rubric_prompt(rubric, &entries);
        let want = ids.clone();
        sample_parsed(
            |idx, p| gen.generate(idx, p),
            s,
            &prompt.text,
            |raw| parse_batch(raw, rubric, &want),
        )
        .map(|mut p| {
            // A fenced collision anywhere in the batch marks the whole batch: with N untrusted
            // documents sharing one context, an injection attempt in any of them is a fact about
            // every verdict that context produced, not just its own case's.
            p.injection_suspected |= prompt.injection_suspected;
            p
        })
    });

    // A provider/transport failure is not attributable to any one case, so it fails the batch and
    // the caller decides whether to retry those cases singly.
    let batched: Vec<Parsed<BatchDims>> = batched.into_iter().collect::<Result<Vec<_>>>()?;

    Ok((0..n)
        .map(|i| {
            let per_case: Vec<Parsed<SampleDims>> =
                batched.iter().map(|b| split_for(b, &ids[i], n)).collect();
            let injection = per_case.iter().any(|p| p.injection_suspected);
            let determinism = per_case
                .iter()
                .fold(Determinism::Exact, |acc, p| acc.weakest(p.determinism));
            let mut outcome = aggregate(&per_case, rubric, model, k as u32, &det[i])?;
            outcome.injection_suspected = injection;
            outcome.determinism = determinism;
            outcome.batch_size = Some(n as u32);
            Ok(outcome)
        })
        .collect())
}

/// Narrow one batched sample to one case: its verdict if the model attributed one, and its share of
/// the call's cost and tokens.
///
/// Cost is divided by the batch: it is the only defensible split of a single indivisible call, and it
/// is why the outcome is marked `batch_size` — a consumer must be able to tell an amortized figure
/// from a measured one. Latency is *not* divided; the batch's wall clock is the honest number for
/// every case in it, since they were produced by one call.
fn split_for(b: &Parsed<BatchDims>, id: &str, n: usize) -> Parsed<SampleDims> {
    let share = n.max(1) as f64;
    Parsed {
        value: b.value.as_ref().and_then(|m| m.get(id).cloned()),
        raw_failure: if b.value.as_ref().is_some_and(|m| m.contains_key(id)) {
            None
        } else {
            // This case got no verdict: report the batch's raw text if the batch itself failed,
            // otherwise say plainly that the response omitted this case.
            Some(
                b.raw_failure.clone().unwrap_or_else(|| {
                    format!("batched judge response carried no verdict for {id}")
                }),
            )
        },
        injection_suspected: b.injection_suspected,
        determinism: b.determinism,
        cost_usd: b.cost_usd.map(|c| c / share),
        latency_ms: b.latency_ms,
        input_tokens: (b.input_tokens as f64 / share).round() as u64,
        output_tokens: (b.output_tokens as f64 / share).round() as u64,
        model: b.model.clone(),
    }
}

#[cfg(test)]
mod tests;
