//! `bench`: dispatch a benchmark run to compare / rubric / simple mode, plus the shared single-output
//! judging helper.

use anyhow::{Context, Result};
use serde_json::json;

use lighttrack_core::{BenchTarget, Benchmark, BenchmarkCase, DatasetItem, ModelPriceRow, Rubric};
use lighttrack_engine::{
    build_eval_prompt, parse_judge_spec, run_judge, run_rubric_judge, EngineConfig,
};

use crate::cli::Cli;
use crate::compare::run_compare;
use crate::http::{get, post};
use crate::rubric::run_rubric_benchmark;
use crate::util::{percentiles, price_gen_cost, run_status};

/// Resolve a benchmark's cases (inline dataset, or a referenced stored dataset) and dispatch to the
/// right mode: comparison (target matrix), rubric (per-dimension), or simple (freeform single score).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_benchmark(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    benchmark_id: &str,
    samples: u32,
    gen_samples: u32,
    heal: bool,
) -> Result<()> {
    let bench: Benchmark = get(cli, http, &format!("/v1/benchmarks/{benchmark_id}"))?;

    let cases: Vec<BenchmarkCase> = if !bench.dataset.is_empty() {
        bench.dataset.clone()
    } else if let Some(ds) = bench.dataset_ref.as_deref() {
        let items: Vec<DatasetItem> = get(cli, http, &format!("/v1/datasets/{ds}/items"))?;
        items
            .into_iter()
            .map(|it| BenchmarkCase {
                input: it.input,
                expected: it.expected,
                output: it.output,
            })
            .collect()
    } else {
        Vec::new()
    };

    let targets: Vec<BenchTarget> = serde_json::from_value(bench.target.clone()).unwrap_or_default();
    if !targets.is_empty() {
        return run_compare(cli, http, engine, &bench, &cases, &targets, samples, gen_samples);
    }
    if let Some(rid) = bench.rubric_id.clone() {
        return run_rubric_benchmark(cli, http, engine, &bench, &cases, &rid, samples, heal);
    }
    run_simple(cli, http, engine, &bench, &cases)
}

/// Simple mode: judge each provided output with a freeform rubric and a single overall score.
fn run_simple(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    bench: &Benchmark,
    cases: &[BenchmarkCase],
) -> Result<()> {
    let (jp, jm) = parse_judge_spec(&bench.judge_model);
    let prices: Vec<ModelPriceRow> = get(cli, http, "/v1/prices").unwrap_or_default();
    println!(
        "benchmark '{}' — {} case(s), judge={jp}/{jm}, baseline={}",
        bench.name,
        cases.len(),
        bench
            .baseline_score
            .map(|b| format!("{b:.3}"))
            .unwrap_or_else(|| "none".into())
    );

    let (mut sum, mut n, mut passes, mut cost) = (0.0_f64, 0u32, 0u32, 0.0_f64);
    let mut latencies: Vec<u64> = Vec::new();
    let mut total_tokens: u64 = 0;
    for (i, case) in cases.iter().enumerate() {
        let output = match &case.output {
            Some(o) => o,
            None => {
                println!("  case {}: skipped (no output)", i + 1);
                continue;
            }
        };
        let prompt = build_eval_prompt(&bench.rubric, &case.input, case.expected.as_deref(), output);
        let outcome = run_judge(engine, &jp, &jm, &prompt).context("judge failed")?;
        let norm = if outcome.verdict.max > 0.0 {
            outcome.verdict.score / outcome.verdict.max
        } else {
            outcome.verdict.score
        };
        sum += norm;
        n += 1;
        if outcome.verdict.pass {
            passes += 1;
        }
        cost += outcome.cost_usd.unwrap_or_else(|| {
            price_gen_cost(&prices, &jp, &jm, outcome.input_tokens, outcome.output_tokens)
        });
        if let Some(l) = outcome.latency_ms {
            latencies.push(l);
        }
        total_tokens += outcome.input_tokens.unwrap_or(0) + outcome.output_tokens.unwrap_or(0);
        println!(
            "  case {}: score={:.2} pass={} {}ms :: {}",
            i + 1,
            norm,
            outcome.verdict.pass,
            outcome.latency_ms.unwrap_or(0),
            outcome.verdict.reasoning
        );
        let score = json!({
            "project_id": bench.project_id,
            "rubric": format!("bench:{}", bench.name),
            "value": outcome.verdict.score, "max": outcome.verdict.max, "pass": outcome.verdict.pass,
            "reasoning": outcome.verdict.reasoning, "scored_by": outcome.model, "cost_usd": outcome.cost_usd,
        });
        post(cli, http, "/v1/scores", &score)?;
    }

    let mean = if n > 0 { sum / n as f64 } else { 0.0 };
    let pass_rate = if n > 0 { passes as f64 / n as f64 } else { 0.0 };
    let (p50, p95) = percentiles(&mut latencies);
    let status = run_status(bench.baseline_score, mean);
    println!(
        "\nscorecard: mean={mean:.3}  pass_rate={:.0}%  cost=${cost:.5}  p50={}ms p95={}ms  tokens={total_tokens}  status={status}",
        pass_rate * 100.0,
        p50.unwrap_or(0),
        p95.unwrap_or(0),
    );
    if let Some(b) = bench.baseline_score {
        let verdict = if status == "regressed" { "REGRESSION" } else { "ok" };
        println!("baseline={b:.3} -> {verdict}");
    }

    let run = json!({
        "benchmark_id": bench.id, "n_cases": n, "mean_score": mean, "pass_rate": pass_rate,
        "cost_usd": cost, "status": status,
        "p50_latency_ms": p50, "p95_latency_ms": p95, "total_tokens": total_tokens,
    });
    let stored = post(cli, http, "/v1/benchmark-runs", &run)?;
    println!("recorded run {}", stored.get("id").and_then(|v| v.as_str()).unwrap_or("?"));
    Ok(())
}

/// One output's judge result, preserving the per-dimension breakdown + self-consistency agreement
/// (so comparison runs can record *why* a score landed where it did, not just the overall).
pub(crate) struct JudgeResult {
    pub(crate) overall: f64,
    pub(crate) pass: bool,
    pub(crate) cost: f64,
    /// Total tokens the judge consumed scoring this output (across samples).
    pub(crate) tokens: u64,
    /// Cross-sample agreement on the overall score (1.0 = identical across samples).
    pub(crate) agreement: f64,
    /// (dimension key, mean score) pairs; empty for freeform-rubric judging.
    pub(crate) dimensions: Vec<(String, f64)>,
}

/// Judge one generated/candidate output via the rubric (if any) or the freeform rubric text, using
/// the configured judge provider/model. Judge cost is priced from the book when the provider gives no $.
#[allow(clippy::too_many_arguments)]
pub(crate) fn judge_output(
    engine: &EngineConfig,
    judge_provider: &str,
    judge_model: &str,
    rubric: &Option<Rubric>,
    bench: &Benchmark,
    case: &BenchmarkCase,
    output: &str,
    samples: u32,
    prices: &[ModelPriceRow],
) -> Result<JudgeResult> {
    if let Some(r) = rubric {
        let o = run_rubric_judge(
            engine, judge_provider, judge_model, r, &case.input,
            case.expected.as_deref(), output, samples,
        )
        .context("rubric judge failed")?;
        let jc = o.cost_usd.unwrap_or_else(|| {
            price_gen_cost(prices, judge_provider, judge_model, o.input_tokens, o.output_tokens)
        });
        Ok(JudgeResult {
            overall: o.overall,
            pass: o.pass,
            cost: jc,
            tokens: o.tokens.unwrap_or(0),
            agreement: o.agreement,
            dimensions: o.dimensions.iter().map(|d| (d.key.clone(), d.score)).collect(),
        })
    } else {
        let prompt = build_eval_prompt(&bench.rubric, &case.input, case.expected.as_deref(), output);
        let v = run_judge(engine, judge_provider, judge_model, &prompt).context("judge failed")?;
        let norm = if v.verdict.max > 0.0 {
            v.verdict.score / v.verdict.max
        } else {
            v.verdict.score
        };
        let jc = v.cost_usd.unwrap_or_else(|| {
            price_gen_cost(prices, judge_provider, judge_model, v.input_tokens, v.output_tokens)
        });
        Ok(JudgeResult {
            overall: norm,
            pass: v.verdict.pass,
            cost: jc,
            tokens: v.input_tokens.unwrap_or(0) + v.output_tokens.unwrap_or(0),
            agreement: 1.0,
            dimensions: Vec::new(),
        })
    }
}
