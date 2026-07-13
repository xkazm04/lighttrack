//! Comparison mode: generate outputs from each target, judge them, compare quality × cost × latency.
//! Records per-dimension breakdown + agreement. With `gen_samples > 1` it generates several
//! candidates per case and averages their scores (generation self-consistency), so a single
//! lucky/unlucky output doesn't dominate — the judge is sampled separately via `samples`.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use serde_json::{json, Map, Value};

use lighttrack_core::{BenchTarget, Benchmark, BenchmarkCase, ModelPriceRow, Rubric};
use lighttrack_engine::{generate, parse_judge_spec, EngineConfig};

use crate::bench::judge_output;
use crate::cli::Cli;
use crate::http::{get, post};
use crate::stats::{annotate_significance, significance_verdict, stability, Summary};
use crate::util::{
    add_price_warnings, aggregate_status, cost_or_book, join_csv, now_ts, parallel_map, percentiles,
};

/// One `(target, case)` cell's independent result: the candidate scores/agreements plus this cell's
/// cost/latency/token contributions. Computed in parallel, then folded in case order so the per-target
/// leaderboard, posted scores, and printed log are byte-identical at any `--jobs`.
struct Cell {
    cand_scores: Vec<f64>,
    judge_agrees: Vec<f64>,
    cand_passes: u32,
    case_dim_sums: HashMap<String, f64>,
    case_judge_cost: f64,
    gen_cost: f64,
    gen_tokens: u64,
    judge_cost: f64,
    judge_tokens: u64,
    latencies: Vec<u64>,
    /// Models with no price-book entry seen while pricing this cell (cost undercounted).
    price_warnings: BTreeSet<String>,
    /// First generation/judge error hit while sampling this cell (printed in the sequential fold).
    error_msg: Option<String>,
}

/// Generate `ng` candidates for one case from one target and judge each; pure (no printing/posting)
/// so it can run concurrently. A generation/judge error stops sampling this cell and is reported back
/// via `error_msg`; whatever candidates already scored are kept (matching the sequential behaviour).
#[allow(clippy::too_many_arguments)]
fn compute_cell(
    engine: &EngineConfig,
    t: &BenchTarget,
    jp: &str,
    jm: &str,
    rubric: &Option<Rubric>,
    bench: &Benchmark,
    case: &BenchmarkCase,
    ng: u32,
    samples: u32,
    prices: &[ModelPriceRow],
) -> Cell {
    let mut cell = Cell {
        cand_scores: Vec::new(),
        judge_agrees: Vec::new(),
        cand_passes: 0,
        case_dim_sums: HashMap::new(),
        case_judge_cost: 0.0,
        gen_cost: 0.0,
        gen_tokens: 0,
        judge_cost: 0.0,
        judge_tokens: 0,
        latencies: Vec::new(),
        price_warnings: BTreeSet::new(),
        error_msg: None,
    };
    for _ in 0..ng {
        let gen = match generate(engine, &t.provider, &t.model, t.system_prompt.as_deref(), &case.input, None) {
            Ok(g) => g,
            Err(e) => {
                cell.error_msg = Some(format!("generation error — {e}"));
                break;
            }
        };
        let (gc, gpriced) =
            cost_or_book(gen.cost_usd, prices, &t.provider, &t.model, gen.input_tokens, gen.output_tokens);
        if !gpriced {
            cell.price_warnings.insert(format!("{}/{}", t.provider, t.model));
        }
        cell.gen_cost += gc;
        cell.gen_tokens += gen.input_tokens.unwrap_or(0) + gen.output_tokens.unwrap_or(0);
        if let Some(l) = gen.latency_ms {
            cell.latencies.push(l);
        }
        let jr = match judge_output(engine, jp, jm, rubric, bench, case, &gen.output, samples, prices) {
            Ok(jr) => jr,
            // Unparseable judge output is not a silent 0.0; stop sampling this cell (and skip the case
            // if none scored) rather than aborting the whole comparison.
            Err(e) => {
                cell.error_msg = Some(format!("judge error — {e}"));
                break;
            }
        };
        if !jr.judge_priced {
            cell.price_warnings.insert(format!("{jp}/{jm}"));
        }
        cell.judge_cost += jr.cost;
        cell.judge_tokens += jr.tokens;
        cell.case_judge_cost += jr.cost;
        cell.cand_scores.push(jr.overall);
        cell.judge_agrees.push(jr.agreement);
        if jr.pass {
            cell.cand_passes += 1;
        }
        for (k, v) in &jr.dimensions {
            *cell.case_dim_sums.entry(k.clone()).or_insert(0.0) += v;
        }
    }
    cell
}

/// One target's leaderboard row: (label, mean, pass_rate, gen_cost, judge_cost, p50_ms, errored, agreement).
/// Round to 3 decimals for compact report JSON.
fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_compare(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    bench: &Benchmark,
    cases: &[BenchmarkCase],
    targets: &[BenchTarget],
    samples: u32,
    gen_samples: u32,
    pairwise: bool,
    jobs: usize,
) -> Result<String> {
    let (jp, jm) = parse_judge_spec(&bench.judge_model);
    let ng = gen_samples.max(1);
    println!(
        "benchmark '{}' COMPARE: {} target(s) × {} case(s), judge={jp}/{jm}, gen_samples={ng}, judge_samples={samples}",
        bench.name,
        targets.len(),
        cases.len(),
    );
    let rubric: Option<Rubric> = match &bench.rubric_id {
        Some(rid) => Some(get(cli, http, &format!("/v1/rubrics/{rid}"))?),
        None => None,
    };
    // For providers whose API doesn't return a $ cost (e.g. Gemini/OpenAI), price by tokens from the DB.
    let prices: Vec<ModelPriceRow> = get(cli, http, "/v1/prices").unwrap_or_default();

    // (label, mean, pass_rate, gen_cost, judge_cost, p50_ms, errored, agreement)
    let mut rows: Vec<(String, f64, f64, f64, f64, u64, u32, f64)> = Vec::new();
    // Per-target verdicts vs the benchmark baseline, rolled up into one honest run-level status below.
    let mut statuses: Vec<String> = Vec::new();
    for t in targets {
        let label = t
            .label
            .clone()
            .unwrap_or_else(|| format!("{}/{}", t.provider, t.model));
        println!("\n-- target {label} --");
        let (mut overall_sum, mut passes, mut judged, mut gen_cost, mut judge_cost, mut errored) =
            (0.0_f64, 0u32, 0u32, 0.0_f64, 0.0_f64, 0u32);
        let mut latencies: Vec<u64> = Vec::new();
        let mut dim_sums: HashMap<String, f64> = HashMap::new();
        let mut agree_sum = 0.0_f64;
        let mut case_reports: Vec<Value> = Vec::new();
        let (mut gen_tokens, mut judge_tokens) = (0u64, 0u64);
        let mut price_warnings: BTreeSet<String> = BTreeSet::new();
        let mut case_scores: Vec<f64> = Vec::new();

        // Generate + judge every case for this target with up to `jobs` concurrency; fold the cells in
        // case order so cost/latency/agreement aggregation is identical to the sequential path.
        let cells: Vec<Cell> = parallel_map(cases.len(), jobs, |i| {
            compute_cell(engine, t, &jp, &jm, &rubric, bench, &cases[i], ng, samples, &prices)
        });

        for (i, cell) in cells.into_iter().enumerate() {
            if let Some(msg) = &cell.error_msg {
                println!("  case {}: {msg}", i + 1);
            }
            price_warnings.extend(cell.price_warnings);
            // Costs/latency/tokens accrue even for an errored (no-candidate) case — the calls still
            // burned tokens and $ before the sampling loop broke.
            gen_cost += cell.gen_cost;
            gen_tokens += cell.gen_tokens;
            judge_cost += cell.judge_cost;
            judge_tokens += cell.judge_tokens;
            latencies.extend(cell.latencies);
            if cell.cand_scores.is_empty() {
                errored += 1;
                continue;
            }

            let n = cell.cand_scores.len() as f64;
            let case_score = cell.cand_scores.iter().sum::<f64>() / n;
            let case_pass = (cell.cand_passes as f64 / n) >= 0.5; // majority of candidates pass
            let gen_agree = stability(&cell.cand_scores);
            let judge_agree = cell.judge_agrees.iter().sum::<f64>() / n;
            // Headline agreement: generation stability when sampling, else the judge's own agreement.
            let case_agree = if ng > 1 { gen_agree } else { judge_agree };

            overall_sum += case_score;
            case_scores.push(case_score);
            agree_sum += case_agree;
            if case_pass {
                passes += 1;
            }
            judged += 1;

            let mut dims_obj = Map::new();
            for (k, s) in &cell.case_dim_sums {
                let dm = s / n;
                *dim_sums.entry(k.clone()).or_insert(0.0) += dm;
                dims_obj.insert(k.clone(), json!(r3(dm)));
            }
            let dim_str: String = dims_obj
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_f64().map(|x| format!("{x:.2}")).unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(" ");
            case_reports.push(json!({
                "case": i + 1, "score": r3(case_score), "pass": case_pass,
                "gen_agreement": r3(gen_agree), "judge_agreement": r3(judge_agree),
                "n_candidates": cell.cand_scores.len(), "dimensions": Value::Object(dims_obj),
            }));
            println!(
                "  case {}: score={:.2} pass={} gen_agree={:.2} judge_agree={:.2} (n_gen={})  {dim_str}",
                i + 1,
                case_score,
                case_pass,
                gen_agree,
                judge_agree,
                cell.cand_scores.len(),
            );
            // Per-case judge verdict → /v1/scores (queryable per case, not just the run aggregate).
            // Best-effort: a transient post failure must not abort a long comparison run.
            let score = json!({
                "project_id": bench.project_id,
                "rubric": format!("{}:{label}#case{}", bench.name, i + 1),
                "value": r3(case_score), "max": 1.0, "pass": case_pass,
                "reasoning": dim_str, "scored_by": format!("{jp}/{jm}"),
                "cost_usd": cell.case_judge_cost,
            });
            let _ = post(cli, http, "/v1/scores", &score);
        }

        let mean = if judged > 0 { overall_sum / judged as f64 } else { 0.0 };
        let pass_rate = if judged > 0 { passes as f64 / judged as f64 } else { 0.0 };
        let mean_agree = if judged > 0 { agree_sum / judged as f64 } else { 1.0 };
        let (p50, p95) = percentiles(&mut latencies);
        rows.push((label.clone(), mean, pass_rate, gen_cost, judge_cost, p50.unwrap_or(0), errored, mean_agree));

        // Per-target verdict vs the benchmark baseline — the flagship multi-target mode now detects
        // regressions (significance-aware) instead of stamping every run "compared". No baseline ⇒
        // "no_baseline"; the CI must exclude the baseline below before a target counts as regressed.
        let summary = Summary::of(&case_scores);
        let (status, scalar_fallback) = if judged > 0 {
            significance_verdict(bench.baseline_score, &summary)
        } else {
            ("no_baseline", false)
        };
        statuses.push(status.to_string());
        if !price_warnings.is_empty() {
            println!("  warning: no price book entry for {} — cost undercounted", join_csv(&price_warnings));
        }

        let dim_means: Map<String, Value> = dim_sums
            .iter()
            .map(|(k, s)| (k.clone(), json!(r3(s / judged.max(1) as f64))))
            .collect();
        let mut report = json!({
            "mode": "compare", "target": label, "provider": t.provider, "model": t.model,
            "prompt_label": t.label, "gen_cost_usd": gen_cost, "judge_cost_usd": judge_cost,
            "gen_tokens": gen_tokens, "judge_tokens": judge_tokens,
            "errored_cases": errored, "gen_samples": ng, "judge_samples": samples,
            "agreement": r3(mean_agree), "dimensions": Value::Object(dim_means), "cases": case_reports,
            "verdict": status, "baseline": bench.baseline_score,
        });
        annotate_significance(&mut report, &summary, scalar_fallback);
        add_price_warnings(&mut report, &price_warnings);
        let run = json!({
            "benchmark_id": bench.id, "n_cases": judged, "mean_score": mean, "pass_rate": pass_rate,
            "cost_usd": gen_cost + judge_cost, "status": status, "finished_at": now_ts(),
            "p50_latency_ms": p50, "p95_latency_ms": p95, "total_tokens": gen_tokens + judge_tokens,
            "report": report,
        });
        post(cli, http, "/v1/benchmark-runs", &run)?;
    }

    // One honest headline status for the whole comparison: regressed if any target regressed.
    let overall = aggregate_status(&statuses.iter().map(String::as_str).collect::<Vec<_>>());
    if bench.baseline_score.is_some() {
        println!("\ncompare verdict vs baseline {:.3}: {overall}", bench.baseline_score.unwrap_or(0.0));
    }

    // Render the leaderboard via the shared render layer, so the runner, CLI, and MCP agree.
    let target_rows: Vec<Value> = rows
        .iter()
        .map(|(label, mean, pr, gc, jc, p50, err, agree)| {
            json!({
                "label": label, "mean": mean, "pass_rate": pr, "agreement": agree,
                "gen_cost_usd": gc, "judge_cost_usd": jc, "p50_latency_ms": p50, "errored": err,
            })
        })
        .collect();
    let summary = json!({ "n_cases": cases.len(), "targets": target_rows, "status": overall });
    match lighttrack_render::render("compare", &summary) {
        Some(md) => println!("\n{md}"),
        None => println!("\n{}", serde_json::to_string_pretty(&summary)?),
    }

    // Optional pairwise phase: printed *alongside* (after) the per-target table, never replacing it.
    if pairwise {
        crate::pairwise::run_pairwise_matrix(
            cli, http, engine, bench, cases, targets, &rubric, &prices, &jp, &jm, jobs,
        )?;
    }
    Ok(overall.to_string())
}

#[cfg(test)]
mod tests {
    use super::r3;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn r3_rounds_to_three_decimals() {
        assert!(approx(r3(0.123456), 0.123));
        assert!(approx(r3(0.123654), 0.124)); // rounds half-away-from-zero at the 4th place
        assert!(approx(r3(1.0), 1.0));
        assert!(approx(r3(0.0), 0.0));
    }
}
