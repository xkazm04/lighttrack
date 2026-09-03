//! Rubric mode: per-dimension judging (with self-consistency), aggregated into a report.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::{Benchmark, BenchmarkCase, ModelPriceRow, Rubric, ScoreKind};
use lighttrack_engine::{
    parse_judge_spec, run_rubric_judge, run_text, Determinism, EngineConfig, RubricOutcome,
};

use std::collections::BTreeSet;

use crate::cli::Cli;
use crate::http::{get, post};
use crate::provenance::{rubric_detail, weakest_reasoning};
use crate::stats::{annotate_significance, significance_verdict, Summary};
use crate::util::{
    add_price_warnings, cost_or_book, dim_mean, join_csv, now_ts, parallel_map, percentiles,
    stamp_determinism,
};

/// One case's judged result: no candidate output, a judged rubric outcome, or an unparseable judge
/// response (carried as a message for the in-order skip log).
pub(crate) enum CaseResult {
    NoOutput,
    Judged(Box<RubricOutcome>),
    Errored(String),
}

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
    jobs: usize,
    // Cases per judge call. `<= 1` judges each case alone (the default and the reference method);
    // higher amortizes the per-call context across a batch. See `crate::batch` for what that
    // changes about the measurement.
    batch: usize,
    report_extra: Option<&serde_json::Value>,
    ctl: &crate::runctl::RunControl,
) -> Result<String> {
    let rubric: Rubric = get(cli, http, &format!("/v1/rubrics/{rubric_id}"))?;
    // Minted before judging so every posted case is run-scoped even if the run post later fails.
    let run_id = lighttrack_core::new_id();
    let (jp, jm) = parse_judge_spec(&bench.judge_model);
    let prices: Vec<ModelPriceRow> = crate::bench::fetch_prices(cli, http);
    // Deterministic dimensions are checked locally: they cost no tokens and are never sampled, so
    // say how many of the rubric's dimensions the judge model is actually paid to score.
    let mechanical = rubric
        .dimensions
        .iter()
        .filter(|d| !d.kind.is_llm())
        .count();
    let dims_note = if mechanical > 0 {
        format!(
            "{} dims, {mechanical} deterministic",
            rubric.dimensions.len()
        )
    } else {
        format!("{} dims", rubric.dimensions.len())
    };
    println!(
        "benchmark '{}' — {} case(s), rubric '{}' ({dims_note}, threshold {:.2}), judge={jp}/{jm}, samples={}",
        bench.name, cases.len(), rubric.name, rubric.threshold, samples
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
    // Verdicts the API refused/couldn't take — recorded on the run instead of scrolling past on stderr.
    let mut score_post_failures = 0u32;
    // Cases whose judged content tried to imitate a prompt boundary (neutralized by the engine's
    // fence) — recorded so an operator can see which scores are attacker-adjacent.
    let mut injected = 0u32;
    // The weakest determinism stamp across every judged case — a run's reproducibility claim is
    // only as strong as its least reproducible verdict.
    let mut determinism = Determinism::Exact;
    let mut price_warnings: BTreeSet<String> = BTreeSet::new();

    // Judge every case with up to `jobs` concurrency (jobs=1 for the engine's per-case sample loop, so
    // total concurrency stays bounded at --jobs). Fold the outcomes in case order so the printed log,
    // posted scores, and scorecard are byte-identical at any `jobs`.
    // A cancellation stops the run at a case boundary: not-yet-started cases are skipped (never
    // interrupted mid-call), and what was judged is kept and marked partial.
    let n_cases = cases.len();
    let results: Vec<CaseResult> = if batch > 1 {
        crate::batch::judge_batched(engine, &jp, &jm, &rubric, cases, samples, jobs, batch, ctl)
    } else {
        parallel_map(n_cases, jobs, |i| {
            if ctl.cancelled() {
                return CaseResult::NoOutput;
            }
            let r = match &cases[i].output {
                None => CaseResult::NoOutput,
                Some(output) => match run_rubric_judge(
                    engine,
                    &jp,
                    &jm,
                    &rubric,
                    &cases[i].input,
                    cases[i].expected.as_deref(),
                    output,
                    samples,
                    1,
                ) {
                    Ok(o) => CaseResult::Judged(Box::new(o)),
                    Err(e) => CaseResult::Errored(e.to_string()),
                },
            };
            ctl.tick(n_cases);
            r
        })
    };
    let cancelled = ctl.cancelled();

    for (i, result) in results.into_iter().enumerate() {
        let o = match result {
            CaseResult::NoOutput => {
                // A case that HAS an output but produced no verdict was skipped by the
                // cancellation, not by missing data — don't blame the dataset for an operator stop.
                let why = if cancelled && cases[i].output.is_some() {
                    "run cancelled"
                } else {
                    "no output"
                };
                println!("  case {} skipped ({why})", i + 1);
                continue;
            }
            // Don't abort the whole run (or record a phantom 0.0) on one garbage judge response —
            // skip the case loudly so the scorecard's denominator stays honest.
            CaseResult::Errored(e) => {
                eprintln!("  case {} skipped — judge output unparseable: {e}", i + 1);
                errored += 1;
                continue;
            }
            CaseResult::Judged(o) => *o,
        };
        judged += 1;
        if o.parse_failures > 0 {
            sample_failures += o.parse_failures;
            eprintln!(
                "  case {}: {}/{} judge samples were unparseable and dropped from the mean",
                i + 1,
                o.parse_failures,
                o.samples
            );
        }
        overall_sum += o.overall;
        scores.push(o.overall);
        if o.pass {
            passes += 1;
        }
        let (jc, priced) = cost_or_book(
            o.cost_usd,
            &prices,
            &jp,
            &jm,
            o.input_tokens,
            o.output_tokens,
        );
        if !priced {
            price_warnings.insert(format!("{jp}/{jm}"));
        }
        cost += jc;
        if let Some(l) = o.latency_ms {
            latencies.push(l);
        }
        total_tokens += o.tokens.unwrap_or(0);
        min_agreement = min_agreement.min(o.agreement);
        determinism = determinism.weakest(o.determinism);
        for d in &o.dimensions {
            *dim_sums.entry(d.key.clone()).or_insert(0.0) += d.score;
        }
        let dim_str = o
            .dimensions
            .iter()
            .map(|d| format!("{}={:.2}", d.key, d.score))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  case {}: overall={:.2} pass={} [{dim_str}]",
            i + 1,
            o.overall,
            o.pass
        );
        if o.injection_suspected {
            injected += 1;
            eprintln!(
                "  case {}: judged content imitated a prompt boundary (neutralized) — treat this \
                 score as attacker-adjacent",
                i + 1
            );
        }
        if !o.pass {
            if let Some(w) = o
                .dimensions
                .iter()
                .min_by(|a, b| a.score.total_cmp(&b.score))
            {
                failing.push(json!({
                    "index": i + 1, "overall": o.overall, "weakest": w.key, "reasoning": w.reasoning()
                }));
            }
        }
        // Post the verdict WITH its provenance: per-dimension values/weights/floors and every
        // sample's reasoning, plus agreement and sample accounting. The one-line `reasoning` quotes
        // the judge's weakest-dimension text instead of restating the rubric's shape.
        let detail = rubric_detail(&o);
        let score = json!({
            "project_id": bench.project_id,
            "rubric": format!("bench:{}", bench.name),
            "kind": ScoreKind::BenchCase.as_str(),
            "rubric_id": bench.rubric_id,
            "run_id": run_id, "case_index": i as u32 + 1,
            "value": o.overall, "max": 1.0, "pass": o.pass,
            "reasoning": weakest_reasoning(&detail),
            "detail": detail,
            "scored_by": o.model, "cost_usd": o.cost_usd,
        });
        // Best-effort (as in compare/simple): a transient post failure is counted, not fatal, and
        // never silent.
        if let Err(e) = post(cli, http, "/v1/scores", &score) {
            eprintln!(
                "  case {}: score post failed (verdict not persisted): {e}",
                i + 1
            );
            score_post_failures += 1;
        }
    }

    let mean = if judged > 0 {
        overall_sum / judged as f64
    } else {
        0.0
    };
    let pass_rate = if judged > 0 {
        passes as f64 / judged as f64
    } else {
        0.0
    };
    let (p50, p95) = percentiles(&mut latencies);

    let dim_means: Vec<Value> = rubric
        .dimensions
        .iter()
        .map(|d| {
            json!({
                "key": d.key, "mean": dim_mean(&dim_sums, &d.key, judged),
                "weight": d.weight, "kind": d.kind.as_str(),
            })
        })
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
            recs.push(format!(
                "Improve '{}' ({}): mean {m:.2} below 0.6.",
                d.key, d.description
            ));
        }
    }
    if mechanical == rubric.dimensions.len() && mechanical > 0 {
        recs.push(
            "Every dimension is deterministic: no judge model was called, so this run spent no \
tokens and its scores are exactly reproducible."
                .to_string(),
        );
    } else if mechanical > 0 && samples > 1 {
        recs.push(format!(
            "{mechanical} of {} dimensions are deterministic (checked locally, zero tokens); \
`agreement` describes only the LLM-judged dimensions.",
            rubric.dimensions.len()
        ));
    }
    if samples > 1 && min_agreement < 0.8 {
        recs.push(format!(
            "Judge agreement dipped to {min_agreement:.2}; tighten anchors or raise --samples."
        ));
    }
    if samples > 1 && determinism == Determinism::BestEffort {
        recs.push(
            "Judge sampling was best-effort (the provider exposes no seed, or none at all), so part of the measured disagreement is sampling noise rather than genuine ambiguity. Set ANTHROPIC_API_KEY for the bare Messages API, or judge on an OpenAI/Gemini model for exact determinism."
                .to_string(),
        );
    }
    if errored > 0 || sample_failures > 0 {
        recs.push(format!(
            "Judge emitted unparseable output: {errored} case(s) skipped, {sample_failures} sample(s) \
dropped. Check the judge model/prompt — these scores are absent, not failing."
        ));
    }
    if injected > 0 {
        recs.push(format!(
            "{injected} case(s) contained content imitating a judge-prompt boundary (neutralized). \
Review those candidates — their scores are attacker-adjacent."
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
        format!(
            "Overall {mean:.2} is below threshold {:.2}.",
            rubric.threshold
        )
    });

    let healing =
        if heal {
            let dims_txt = rubric
                .dimensions
                .iter()
                .map(|d| {
                    format!(
                        "{} (w{}) mean {:.2}",
                        d.key,
                        d.weight,
                        dim_mean(&dim_sums, &d.key, judged)
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            let prompt =
                format!(
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
    let (verdict_status, scalar_fallback) = significance_verdict(bench.baseline_score, &summary);
    // A cancelled run judged only part of its dataset — it never reads as a finished one.
    let status = if cancelled {
        "cancelled"
    } else {
        verdict_status
    };

    let mut report = json!({
        "rubric": rubric.name, "threshold": rubric.threshold, "samples": samples,
        "overall_mean": mean, "pass_rate": pass_rate, "dimensions": dim_means,
        "weakest_dimension": weakest, "recommendations": recs,
        "unparseable_cases": errored, "dropped_samples": sample_failures,
        "injection_suspected_cases": injected, "score_post_failures": score_post_failures,
        "cancelled": cancelled, "partial": cancelled, "cases_planned": n_cases,
    });
    // Rubric mode judges outputs the caller supplied — it generates nothing, so the generation half
    // of the stamp is `null` rather than a claim. The headline `determinism` is unchanged for this
    // mode; only its shape is now shared with compare/pairwise.
    stamp_determinism(&mut report, None, (judged > 0).then_some(determinism));
    // Bounded, with the truncation signal beside it — an unbounded array here is a report blob that
    // grows with the dataset. The complete per-case record is the run's scores.
    crate::bench::attach_cases(&mut report, "failing_cases", failing);
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

    crate::bench::stamp_pins(&mut report, bench, report_extra);
    let run = json!({
        "id": run_id,
        "benchmark_id": bench.id, "n_cases": judged, "mean_score": mean, "pass_rate": pass_rate,
        "cost_usd": cost, "status": status, "finished_at": now_ts(),
        "p50_latency_ms": p50, "p95_latency_ms": p95, "total_tokens": total_tokens, "report": report,
    });
    let stored = post(cli, http, "/v1/benchmark-runs", &run)?;
    println!(
        "\nrecorded run {}",
        stored.get("id").and_then(|v| v.as_str()).unwrap_or("?")
    );
    Ok(status.to_string())
}
