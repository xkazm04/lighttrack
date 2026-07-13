//! Rubric mode: per-dimension judging (with self-consistency), aggregated into a report.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::{Benchmark, BenchmarkCase, ModelPriceRow, Rubric};
use lighttrack_engine::{parse_judge_spec, run_rubric_judge, run_text, EngineConfig};

use std::collections::BTreeSet;

use crate::cli::Cli;
use crate::http::{get, post};
use crate::stats::{annotate_significance, significance_verdict, Summary};
use crate::util::{add_price_warnings, cost_or_book, dim_mean, join_csv, now_ts, percentiles};

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_rubric_benchmark(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    bench: &Benchmark,
    cases: &[BenchmarkCase],
    rubric_id: &str,
    samples: u32,
    heal: bool,
) -> Result<String> {
    let rubric: Rubric = get(cli, http, &format!("/v1/rubrics/{rubric_id}"))?;
    let (jp, jm) = parse_judge_spec(&bench.judge_model);
    let prices: Vec<ModelPriceRow> = get(cli, http, "/v1/prices").unwrap_or_default();
    println!(
        "benchmark '{}' — {} case(s), rubric '{}' ({} dims, threshold {:.2}), judge={jp}/{jm}, samples={}",
        bench.name, cases.len(), rubric.name, rubric.dimensions.len(), rubric.threshold, samples
    );

    let mut dim_sums: HashMap<String, f64> = HashMap::new();
    let mut overall_sum = 0.0_f64;
    let mut scores: Vec<f64> = Vec::new();
    let (mut passes, mut judged, mut total_tokens) = (0u32, 0u32, 0u64);
    let mut cost = 0.0_f64;
    let mut latencies: Vec<u64> = Vec::new();
    let mut min_agreement = 1.0_f64;
    let mut failing: Vec<Value> = Vec::new();
    // Cases whose judge output was wholly unparseable (skipped, never scored), and the running tally
    // of individual self-consistency samples dropped from scored cases — kept out of the means so a
    // flaky judge response never silently records a 0.0.
    let (mut errored, mut sample_failures) = (0u32, 0u32);
    let mut price_warnings: BTreeSet<String> = BTreeSet::new();

    for (i, case) in cases.iter().enumerate() {
        let output = match &case.output {
            Some(o) => o,
            None => {
                println!("  case {} skipped (no output)", i + 1);
                continue;
            }
        };
        let o = match run_rubric_judge(
            engine, &jp, &jm, &rubric, &case.input, case.expected.as_deref(), output, samples,
        ) {
            Ok(o) => o,
            // Don't abort the whole run (or record a phantom 0.0) on one garbage judge response —
            // skip the case loudly so the scorecard's denominator stays honest.
            Err(e) => {
                eprintln!("  case {} skipped — judge output unparseable: {e}", i + 1);
                errored += 1;
                continue;
            }
        };
        judged += 1;
        if o.parse_failures > 0 {
            sample_failures += o.parse_failures;
            eprintln!(
                "  case {}: {}/{} judge samples were unparseable and dropped from the mean",
                i + 1, o.parse_failures, o.samples
            );
        }
        overall_sum += o.overall;
        scores.push(o.overall);
        if o.pass {
            passes += 1;
        }
        let (jc, priced) = cost_or_book(o.cost_usd, &prices, &jp, &jm, o.input_tokens, o.output_tokens);
        if !priced {
            price_warnings.insert(format!("{jp}/{jm}"));
        }
        cost += jc;
        if let Some(l) = o.latency_ms {
            latencies.push(l);
        }
        total_tokens += o.tokens.unwrap_or(0);
        min_agreement = min_agreement.min(o.agreement);
        for d in &o.dimensions {
            *dim_sums.entry(d.key.clone()).or_insert(0.0) += d.score;
        }
        let dim_str = o
            .dimensions
            .iter()
            .map(|d| format!("{}={:.2}", d.key, d.score))
            .collect::<Vec<_>>()
            .join(" ");
        println!("  case {}: overall={:.2} pass={} [{dim_str}]", i + 1, o.overall, o.pass);
        if !o.pass {
            if let Some(w) = o.dimensions.iter().min_by(|a, b| a.score.total_cmp(&b.score)) {
                failing.push(json!({
                    "index": i + 1, "overall": o.overall, "weakest": w.key, "reasoning": w.reasoning
                }));
            }
        }
        let score = json!({
            "project_id": bench.project_id,
            "rubric": format!("bench:{}", bench.name),
            "value": o.overall, "max": 1.0, "pass": o.pass,
            "reasoning": format!("rubric '{}' overall over {} dims", rubric.name, o.dimensions.len()),
            "scored_by": o.model, "cost_usd": o.cost_usd,
        });
        post(cli, http, "/v1/scores", &score)?;
    }

    let mean = if judged > 0 { overall_sum / judged as f64 } else { 0.0 };
    let pass_rate = if judged > 0 { passes as f64 / judged as f64 } else { 0.0 };
    let (p50, p95) = percentiles(&mut latencies);

    let dim_means: Vec<Value> = rubric
        .dimensions
        .iter()
        .map(|d| json!({ "key": d.key, "mean": dim_mean(&dim_sums, &d.key, judged), "weight": d.weight }))
        .collect();
    let weakest = rubric
        .dimensions
        .iter()
        .map(|d| (d.key.clone(), dim_mean(&dim_sums, &d.key, judged)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(k, _)| k);

    let mut recs: Vec<String> = Vec::new();
    if let Some(w) = &weakest {
        recs.push(format!(
            "Weakest dimension '{w}' (mean {:.2}); {}/{judged} cases failed.",
            dim_mean(&dim_sums, w, judged),
            judged - passes
        ));
    }
    for d in &rubric.dimensions {
        let m = dim_mean(&dim_sums, &d.key, judged);
        if m < 0.6 {
            recs.push(format!("Improve '{}' ({}): mean {m:.2} below 0.6.", d.key, d.description));
        }
    }
    if samples > 1 && min_agreement < 0.8 {
        recs.push(format!(
            "Judge agreement dipped to {min_agreement:.2}; tighten anchors or raise --samples."
        ));
    }
    if errored > 0 || sample_failures > 0 {
        recs.push(format!(
            "Judge emitted unparseable output: {errored} case(s) skipped, {sample_failures} sample(s) \
dropped. Check the judge model/prompt — these scores are absent, not failing."
        ));
    }
    if !price_warnings.is_empty() {
        recs.push(format!(
            "No price-book entry for {}; judge cost is undercounted (seed config/pricing.json).",
            join_csv(&price_warnings)
        ));
    }
    recs.push(if mean >= rubric.threshold {
        format!("Overall {mean:.2} meets threshold {:.2}.", rubric.threshold)
    } else {
        format!("Overall {mean:.2} is below threshold {:.2}.", rubric.threshold)
    });

    let healing = if heal {
        let dims_txt = rubric
            .dimensions
            .iter()
            .map(|d| format!("{} (w{}) mean {:.2}", d.key, d.weight, dim_mean(&dim_sums, &d.key, judged)))
            .collect::<Vec<_>>()
            .join("; ");
        let prompt = format!(
            "You are an LLM evaluation consultant. Benchmark '{}' scored overall {mean:.2} \
(threshold {:.2}, pass rate {:.0}%). Per-dimension means: {dims_txt}. {} of {judged} cases failed. \
In 3-5 concise bullet points, recommend concrete fixes (prompt changes, model choice, rubric \
clarifications) targeting the weakest dimensions. Return only the bullets.",
            bench.name, rubric.threshold, pass_rate * 100.0, judged - passes
        );
        match run_text(engine, &prompt) {
            Ok(t) => Some(t.text.trim().to_string()),
            Err(e) => {
                eprintln!("healing pass failed: {e}");
                None
            }
        }
    } else {
        None
    };

    // Significance-aware verdict (CI-excludes-baseline for n≥2, scalar fallback otherwise).
    let summary = Summary::of(&scores);
    let (status, scalar_fallback) = significance_verdict(bench.baseline_score, &summary);

    let mut report = json!({
        "rubric": rubric.name, "threshold": rubric.threshold, "samples": samples,
        "overall_mean": mean, "pass_rate": pass_rate, "dimensions": dim_means,
        "weakest_dimension": weakest, "failing_cases": failing, "recommendations": recs,
        "unparseable_cases": errored, "dropped_samples": sample_failures,
    });
    if let Some(h) = &healing {
        report["healing"] = json!(h);
    }
    annotate_significance(&mut report, &summary, scalar_fallback);
    add_price_warnings(&mut report, &price_warnings);

    println!(
        "\nscorecard: overall={mean:.3}±{:.3} (n={})  pass_rate={:.0}%  cost=${cost:.5}  p50={}ms  tokens={total_tokens}  unparseable={errored}  status={status}",
        summary.stderr,
        summary.n,
        pass_rate * 100.0,
        p50.unwrap_or(0)
    );
    print!("dimensions:");
    for d in &rubric.dimensions {
        print!("  {}={:.2}", d.key, dim_mean(&dim_sums, &d.key, judged));
    }
    println!();
    if let Some(w) = &weakest {
        println!("weakest: {w}");
    }
    println!("recommendations:");
    for r in &recs {
        println!("  - {r}");
    }
    if let Some(h) = &healing {
        println!("\nhealing:\n{h}");
    }

    let run = json!({
        "benchmark_id": bench.id, "n_cases": judged, "mean_score": mean, "pass_rate": pass_rate,
        "cost_usd": cost, "status": status, "finished_at": now_ts(),
        "p50_latency_ms": p50, "p95_latency_ms": p95, "total_tokens": total_tokens, "report": report,
    });
    let stored = post(cli, http, "/v1/benchmark-runs", &run)?;
    println!("\nrecorded run {}", stored.get("id").and_then(|v| v.as_str()).unwrap_or("?"));
    Ok(status.to_string())
}
