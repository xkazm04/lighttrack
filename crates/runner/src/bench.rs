//! `bench`: dispatch a benchmark run to compare / rubric / simple mode, plus the shared single-output
//! judging helper.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use lighttrack_core::{
    BenchTarget, Benchmark, BenchmarkCase, Dataset, DatasetItem, ModelPriceRow, Rubric,
    ScoreDetail, ScoreKind, RESOLVED_PROMPT_VERSION,
};
use lighttrack_engine::{
    build_eval_prompt, parse_judge_spec, run_judge, run_rubric_judge, Determinism, EngineConfig,
    JudgeOutcome,
};

use crate::cli::Cli;
use crate::compare::run_compare;
use crate::http::{get, post};
use crate::provenance::{freeform_detail, rubric_detail};
use crate::rubric::run_rubric_benchmark;
use crate::runctl::RunControl;
use crate::stats::{annotate_significance, significance_verdict, Summary};
use crate::targets::{resolve_targets, run_resolved_version};
use crate::util::{
    add_price_warnings, cost_or_book, join_csv, now_ts, parallel_map, percentiles,
    stamp_determinism,
};

/// Parse a benchmark's `target` field into a comparison matrix. An **array** is unambiguously a
/// target matrix and must deserialize cleanly — a malformed one is a hard error, not a silent
/// fallthrough to a different mode. Any non-array (null / object / string) is legacy free-form
/// `target` and yields no comparison targets.
pub(crate) fn parse_targets(target: &Value) -> Result<Vec<BenchTarget>> {
    if target.is_array() {
        serde_json::from_value(target.clone()).context(
            "benchmark `target` is an array but not a valid comparison matrix \
             (expected [{provider, model, system_prompt?, label?, prompt_ref?, kind?}, ...])",
        )
    } else {
        Ok(Vec::new())
    }
}

/// Upper bound on an inline per-case array in a run report. A run report is read whole (and stored
/// in one column), so a 5000-case benchmark must not write a 5000-element blob into it. The full
/// per-case record is not lost — it lives in the run's scores (`GET /v1/scores?run=<id>`); the
/// report keeps the first `MAX_LOGGED_CASES` as a preview, and [`attach_cases`] says so explicitly.
pub(crate) const MAX_LOGGED_CASES: usize = 200;

/// Attach a bounded per-case array to a run report under `key`, with the truncation signal beside
/// it: `{key}_total` (how many there were), `{key}_logged` (how many are here) and
/// `{key}_truncated`. A consumer must never be able to mistake a clipped list for a complete one —
/// which is exactly what an unbounded-then-silently-capped array would do.
pub(crate) fn attach_cases(report: &mut Value, key: &str, mut all: Vec<Value>) {
    let total = all.len();
    let truncated = total > MAX_LOGGED_CASES;
    all.truncate(MAX_LOGGED_CASES);
    if let Value::Object(m) = report {
        m.insert(format!("{key}_total"), json!(total));
        m.insert(format!("{key}_logged"), json!(all.len()));
        m.insert(format!("{key}_truncated"), json!(truncated));
        m.insert(key.to_string(), Value::Array(all));
    }
}

/// Merge the dataset's **content pin** into a run's `report_extra`: `dataset_frozen` and
/// `dataset_version` as of run time. `dataset_ref` alone pins an *id*, and a dataset is mutable
/// until someone freezes it (freezing is opt-in — see `api::datasets::freeze_dataset`), so two runs
/// citing the same ref can have been scored on different cases. Recording the truth is not the same
/// as changing the policy: an unfrozen dataset still runs, it just no longer *reads* as pinned.
pub(crate) fn dataset_pin(extra: Option<&Value>, ds: &Dataset) -> Value {
    let mut m = match extra {
        Some(Value::Object(o)) => o.clone(),
        _ => serde_json::Map::new(),
    };
    m.insert("dataset_frozen".into(), json!(ds.frozen));
    m.insert("dataset_version".into(), json!(ds.version));
    Value::Object(m)
}

/// Stamp reproducibility pins into a run report: what judged (`judge_model`), against what
/// (`rubric_id` / `dataset_ref`, when the benchmark has them), plus caller-supplied provenance —
/// e.g. the `{prompt_id, prompt_version}` a version-triggered run was scoring, which is what makes
/// the promotion gate version-aware. A run that pins nothing cannot be re-run as published.
pub(crate) fn stamp_pins(report: &mut Value, bench: &Benchmark, extra: Option<&Value>) {
    if let Value::Object(m) = report {
        m.insert("judge_model".into(), json!(bench.judge_model));
        if let Some(r) = &bench.rubric_id {
            m.insert("rubric_id".into(), json!(r));
        }
        if let Some(d) = &bench.dataset_ref {
            m.insert("dataset_ref".into(), json!(d));
        }
        if let Some(Value::Object(e)) = extra {
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Resolve a benchmark's cases (inline dataset, or a referenced stored dataset) and dispatch to the
/// right mode: comparison (target matrix), rubric (per-dimension), or simple (freeform single score).
/// Run a benchmark and return its run-level status (`passed` | `regressed` | `no_baseline`), which
/// `--gate` maps to an exit code. Compare mode returns the aggregate across targets. `jobs` bounds
/// concurrency across cases (or compare cells). `report_extra` is merged into the run report by
/// [`stamp_pins`] (provenance, e.g. the prompt version a version-triggered run scores).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_benchmark(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    benchmark_id: &str,
    samples: u32,
    gen_samples: u32,
    batch: usize,
    heal: bool,
    pairwise: bool,
    jobs: usize,
    report_extra: Option<&Value>,
    ctl: &RunControl,
) -> Result<String> {
    let bench: Benchmark = get(cli, http, &format!("/v1/benchmarks/{benchmark_id}"))?;

    // A referenced dataset also contributes its frozen-state + version to the run's provenance, so
    // "which cases was this scored on?" is answerable from the run alone.
    let mut extra_owned: Option<Value> = None;
    let cases: Vec<BenchmarkCase> = if !bench.dataset.is_empty() {
        bench.dataset.clone()
    } else if let Some(ds) = bench.dataset_ref.as_deref() {
        let items: Vec<DatasetItem> = get(cli, http, &format!("/v1/datasets/{ds}/items"))?;
        match get::<Dataset>(cli, http, &format!("/v1/datasets/{ds}")) {
            Ok(d) => {
                if !d.frozen {
                    println!(
                        "  note: dataset {ds} (v{}) is NOT frozen — its cases can change between \
                         runs, so this run is not exactly reproducible from its ref alone.",
                        d.version
                    );
                }
                extra_owned = Some(dataset_pin(report_extra, &d));
            }
            // The items already loaded; a missing/forbidden dataset head is a provenance gap, not a
            // reason to abandon a paid run. Say so instead of silently claiming a pin.
            Err(e) => eprintln!(
                "  warning: could not read dataset {ds} metadata ({e}); the run \
                                 will not record its frozen-state/version"
            ),
        }
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
    let report_extra = extra_owned.as_ref().or(report_extra);

    let targets = parse_targets(&bench.target)?;
    if !targets.is_empty() {
        // Resolve every target's registry prompt BEFORE the first paid call: a run that cannot
        // fetch what it is supposed to be testing must fail whole, not half-spent. The version
        // override rides in the report_extra the version-triggered enqueue stamped.
        let override_version = report_extra.and_then(|e| {
            let name = e.get("prompt_name")?.as_str()?;
            let v = e.get("prompt_version")?.as_u64()? as u32;
            Some((name, v))
        });
        let resolved = resolve_targets(cli, http, &bench.project_id, &targets, override_version)?;
        // What the run ACTUALLY generated with — the evidence the promotion gate requires, and the
        // one report key no provenance passthrough can fake.
        let mut extra_with_version = None;
        if let Some(v) = run_resolved_version(&resolved, override_version.map(|(n, _)| n)) {
            let mut m = match report_extra {
                Some(Value::Object(o)) => o.clone(),
                _ => serde_json::Map::new(),
            };
            m.insert(RESOLVED_PROMPT_VERSION.into(), json!(v));
            extra_with_version = Some(Value::Object(m));
        }
        let report_extra = extra_with_version.as_ref().or(report_extra);
        return run_compare(
            cli,
            http,
            engine,
            &bench,
            &cases,
            &resolved,
            samples,
            gen_samples,
            pairwise,
            jobs,
            report_extra,
            ctl,
        );
    }
    if let Some(rid) = bench.rubric_id.clone() {
        return run_rubric_benchmark(
            cli,
            http,
            engine,
            &bench,
            &cases,
            &rid,
            samples,
            heal,
            jobs,
            batch,
            report_extra,
            ctl,
        );
    }
    run_simple(cli, http, engine, &bench, &cases, jobs, report_extra, ctl)
}

/// Simple mode: judge each provided output with a freeform rubric and a single overall score. Cases
/// are judged with up to `jobs` concurrency; printing/posting/aggregation stay in case order.
// Deferred, not waived: the params-struct cleanup CLAUDE.md asks for only pays off if it covers all
// four `run_benchmark` dispatch targets at once — `run_compare` (12 args) and `run_rubric_benchmark`
// (11 args) take the same leading arguments positionally. Threading a shared context through just
// this one would leave the siblings inconsistent, so it belongs in its own reviewed change rather
// than in a mechanical lint pass.
#[allow(clippy::too_many_arguments)]
fn run_simple(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    bench: &Benchmark,
    cases: &[BenchmarkCase],
    jobs: usize,
    report_extra: Option<&Value>,
    ctl: &RunControl,
) -> Result<String> {
    let (jp, jm) = parse_judge_spec(&bench.judge_model);
    let prices: Vec<ModelPriceRow> = fetch_prices(cli, http);
    // Mint the run id BEFORE judging so every case posted below is already run-scoped. Deriving it
    // afterwards from the stored run would leave the cases orphaned whenever the run post fails.
    let run_id = lighttrack_core::new_id();
    println!(
        "benchmark '{}' — {} case(s), judge={jp}/{jm}, baseline={}",
        bench.name,
        cases.len(),
        bench
            .baseline_score
            .map(|b| format!("{b:.3}"))
            .unwrap_or_else(|| "none".into())
    );

    // Judge each case with output in parallel; `None` marks a skipped (no-output) case so the
    // sequential fold below prints/aggregates exactly as the old in-order loop did.
    // A cancelled run stops at a case boundary: cases not yet started are skipped (`None`, counted
    // below), never abandoned mid-call, and whatever was judged is kept and marked partial.
    let n_cases = cases.len();
    let judged: Vec<Result<Option<JudgeOutcome>>> = parallel_map(n_cases, jobs, |i| {
        if ctl.cancelled() {
            return Ok(None);
        }
        let out = match &cases[i].output {
            None => Ok(None),
            Some(output) => {
                let prompt = build_eval_prompt(
                    &bench.rubric,
                    &cases[i].input,
                    cases[i].expected.as_deref(),
                    output,
                );
                run_judge(engine, &jp, &jm, &prompt)
                    .context("judge failed")
                    .map(Some)
            }
        };
        ctl.tick(n_cases);
        out
    });
    let cancelled = ctl.cancelled();

    let (mut sum, mut n, mut passes, mut cost) = (0.0_f64, 0u32, 0u32, 0.0_f64);
    let mut latencies: Vec<u64> = Vec::new();
    let mut total_tokens: u64 = 0;
    let mut price_warnings: BTreeSet<String> = BTreeSet::new();
    let mut scores: Vec<f64> = Vec::new();
    // Simple mode used to persist nothing per case beyond a run aggregate — the per-case detail
    // existed only in stdout. It now records both: a bounded preview in the report, and the full
    // run-scoped verdicts in `scores` (queryable via `GET /v1/scores?run=`).
    let mut case_reports: Vec<Value> = Vec::new();
    let mut score_post_failures = 0u32;
    // The weakest judging determinism across the cases actually judged — the run's reproducibility is
    // its worst case's, and the collective digest reads this stamp to tell a pinned run from a
    // sampled one. Compare/rubric/pairwise modes already stamp it; simple mode used to drop it.
    let mut determinism: Option<Determinism> = None;
    for (i, outcome) in judged.into_iter().enumerate() {
        let outcome = match outcome? {
            Some(o) => o,
            None => {
                // A case with an output that produced no verdict was skipped by the cancellation,
                // not by missing data — the log must not blame the dataset for an operator's stop.
                let why = if cancelled && cases[i].output.is_some() {
                    "run cancelled"
                } else {
                    "no output"
                };
                println!("  case {}: skipped ({why})", i + 1);
                continue;
            }
        };
        let norm = if outcome.verdict.max > 0.0 {
            outcome.verdict.score / outcome.verdict.max
        } else {
            outcome.verdict.score
        };
        determinism = Some(determinism.map_or(outcome.determinism, |prev| {
            prev.weakest(outcome.determinism)
        }));
        sum += norm;
        scores.push(norm);
        n += 1;
        if outcome.verdict.pass {
            passes += 1;
        }
        let (jc, priced) = cost_or_book(
            outcome.cost_usd,
            &prices,
            &jp,
            &jm,
            outcome.input_tokens,
            outcome.output_tokens,
        );
        if !priced {
            price_warnings.insert(format!("{jp}/{jm}"));
        }
        cost += jc;
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
        case_reports.push(json!({
            "case": i + 1, "score": norm, "pass": outcome.verdict.pass,
            "latency_ms": outcome.latency_ms,
        }));
        let score = json!({
            "project_id": bench.project_id,
            "rubric": format!("bench:{}", bench.name),
            // The typed identity beside the legacy label: what sort of verdict this is, and
            // which rubric it cites. Without them the label is the only handle, and a label
            // is neither stable across a rename nor unique across two rubrics.
            "kind": ScoreKind::BenchCase.as_str(),
            "rubric_id": bench.rubric_id,
            "run_id": run_id, "case_index": i as u32 + 1,
            "value": outcome.verdict.score, "max": outcome.verdict.max, "pass": outcome.verdict.pass,
            "reasoning": outcome.verdict.reasoning, "scored_by": outcome.model, "cost_usd": outcome.cost_usd,
            "detail": freeform_detail(&outcome),
        });
        // Best-effort, as in compare mode: a transient post failure must neither abort a long run nor
        // vanish — it is counted into the report so "the cases are missing" is a recorded fact.
        if let Err(e) = post(cli, http, "/v1/scores", &score) {
            eprintln!(
                "  case {}: score post failed (verdict not persisted): {e}",
                i + 1
            );
            score_post_failures += 1;
        }
    }

    let mean = if n > 0 { sum / n as f64 } else { 0.0 };
    let pass_rate = if n > 0 { passes as f64 / n as f64 } else { 0.0 };
    let (p50, p95) = percentiles(&mut latencies);
    // Significance-aware verdict: a regression needs the whole ~95% CI below baseline (n≥2), else a
    // scalar fallback. Same regressed/passed/no_baseline vocabulary as the other modes.
    let summary = Summary::of(&scores);
    // No verdict was produced (every case lacked an output, or was skipped) → there is nothing to
    // hold against the baseline. Passing the baseline anyway let the n=0 scalar fallback compare a
    // mean of 0.0 to it and publish `regressed` over zero cases; compare mode already withholds it.
    let baseline = bench.baseline_score.filter(|_| n > 0);
    let (verdict_status, scalar_fallback) = significance_verdict(baseline, &summary);
    // A cancelled run judged only part of its dataset — it must never be published under a verdict
    // that reads as a finished one.
    let status = if cancelled {
        "cancelled"
    } else {
        verdict_status
    };
    println!(
        "\nscorecard: mean={mean:.3}±{:.3} (n={})  pass_rate={:.0}%  cost=${cost:.5}  p50={}ms p95={}ms  tokens={total_tokens}  status={status}",
        summary.stderr,
        summary.n,
        pass_rate * 100.0,
        p50.unwrap_or(0),
        p95.unwrap_or(0),
    );
    if let Some(b) = bench.baseline_score {
        let verdict = if status == "regressed" {
            "REGRESSION"
        } else {
            "ok"
        };
        println!("baseline={b:.3} -> {verdict}");
    }
    if !price_warnings.is_empty() {
        println!(
            "warning: no price book entry for {} — judge cost undercounted",
            join_csv(&price_warnings)
        );
    }

    if cancelled {
        println!(
            "\nCANCELLED: stopped at a case boundary after {n} of {n_cases} case(s); the results \
             above are PARTIAL.",
            n = summary.n,
        );
    }
    let mut report = json!({
        "mode": "simple", "score_post_failures": score_post_failures,
        "cancelled": cancelled, "partial": cancelled, "cases_planned": n_cases,
    });
    attach_cases(&mut report, "cases", case_reports);
    annotate_significance(&mut report, &summary, scalar_fallback);
    // Simple mode judges pre-existing outputs, so only the judging half is ours to claim; the
    // generation half is `null` rather than an invented one.
    stamp_determinism(&mut report, None, determinism);
    add_price_warnings(&mut report, &price_warnings);
    stamp_pins(&mut report, bench, report_extra);
    let run = json!({
        "id": run_id,
        "benchmark_id": bench.id, "n_cases": n, "mean_score": mean, "pass_rate": pass_rate,
        "cost_usd": cost, "status": status, "finished_at": now_ts(),
        "p50_latency_ms": p50, "p95_latency_ms": p95, "total_tokens": total_tokens,
        "report": report,
    });
    let stored = post(cli, http, "/v1/benchmark-runs", &run)?;
    println!(
        "recorded run {}",
        stored.get("id").and_then(|v| v.as_str()).unwrap_or("?")
    );
    Ok(status.to_string())
}

/// The price book, or an empty one with the reason said out loud. `unwrap_or_default()` here made
/// an unreachable API and a missing book entry indistinguishable: every model then surfaced as
/// "no price book entry for …" and an operator went looking at the book instead of at the network.
pub(crate) fn fetch_prices(cli: &Cli, http: &reqwest::blocking::Client) -> Vec<ModelPriceRow> {
    get(cli, http, "/v1/prices").unwrap_or_else(|e| {
        eprintln!(
            "  warning: could not read the price book ({e}); every model will be reported as \
             unpriced and this run's cost will be undercounted"
        );
        Vec::new()
    })
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
    /// False when the judge model had no price-book entry and its cost fell back to 0 (book miss).
    pub(crate) judge_priced: bool,
    /// (dimension key, mean score) pairs; empty for freeform-rubric judging.
    pub(crate) dimensions: Vec<(String, f64)>,
    /// Structured provenance for this verdict — per-dimension values/floors, every sample's
    /// reasoning, agreement and sample accounting. Posted with the score instead of being dropped.
    pub(crate) detail: ScoreDetail,
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
        // jobs=1: compare parallelizes across (target, case) cells, so per-cell sample judging stays
        // sequential to keep total concurrency bounded at --jobs.
        let o = run_rubric_judge(
            engine,
            judge_provider,
            judge_model,
            r,
            &case.input,
            case.expected.as_deref(),
            output,
            samples,
            1,
        )
        .context("rubric judge failed")?;
        let (jc, priced) = cost_or_book(
            o.cost_usd,
            prices,
            judge_provider,
            judge_model,
            o.input_tokens,
            o.output_tokens,
        );
        Ok(JudgeResult {
            overall: o.overall,
            pass: o.pass,
            cost: jc,
            tokens: o.tokens.unwrap_or(0),
            agreement: o.agreement,
            judge_priced: priced,
            dimensions: o
                .dimensions
                .iter()
                .map(|d| (d.key.clone(), d.score))
                .collect(),
            detail: rubric_detail(&o),
        })
    } else {
        let prompt =
            build_eval_prompt(&bench.rubric, &case.input, case.expected.as_deref(), output);
        let v = run_judge(engine, judge_provider, judge_model, &prompt).context("judge failed")?;
        let norm = if v.verdict.max > 0.0 {
            v.verdict.score / v.verdict.max
        } else {
            v.verdict.score
        };
        let (jc, priced) = cost_or_book(
            v.cost_usd,
            prices,
            judge_provider,
            judge_model,
            v.input_tokens,
            v.output_tokens,
        );
        Ok(JudgeResult {
            overall: norm,
            pass: v.verdict.pass,
            cost: jc,
            tokens: v.input_tokens.unwrap_or(0) + v.output_tokens.unwrap_or(0),
            agreement: 1.0,
            judge_priced: priced,
            detail: freeform_detail(&v),
            dimensions: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{dataset_pin, parse_targets, stamp_pins};
    use crate::util::{add_price_warnings, join_csv};
    use lighttrack_core::Dataset;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn dataset(version: u32, frozen: bool) -> Dataset {
        serde_json::from_value(json!({ "name": "d", "version": version, "frozen": frozen }))
            .unwrap()
    }

    #[test]
    fn dataset_pin_records_frozen_state_and_version() {
        // No caller provenance → the pin alone.
        let p = dataset_pin(None, &dataset(3, true));
        assert_eq!(p["dataset_frozen"], json!(true));
        assert_eq!(p["dataset_version"], json!(3));
        // Caller provenance (the prompt version a gated run scores) survives the merge.
        let extra = json!({ "prompt_id": "p1", "prompt_version": 9 });
        let p = dataset_pin(Some(&extra), &dataset(1, false));
        assert_eq!(p["prompt_id"], json!("p1"));
        assert_eq!(p["prompt_version"], json!(9));
        assert_eq!(
            p["dataset_frozen"],
            json!(false),
            "an unfrozen dataset is recorded, not hidden"
        );
    }

    #[test]
    fn stamp_pins_carries_the_dataset_pin_into_the_report() {
        let bench: lighttrack_core::Benchmark =
            serde_json::from_value(json!({ "name": "b", "rubric": "r", "dataset_ref": "ds1" }))
                .unwrap();
        let mut report = json!({ "mode": "compare" });
        let extra = dataset_pin(None, &dataset(2, false));
        stamp_pins(&mut report, &bench, Some(&extra));
        assert_eq!(
            report["dataset_ref"],
            json!("ds1"),
            "the id is still pinned"
        );
        assert_eq!(
            report["dataset_version"],
            json!(2),
            "…and now so is the content it named"
        );
        assert_eq!(report["dataset_frozen"], json!(false));
    }

    #[test]
    fn parse_targets_null_and_object_are_no_matrix() {
        assert!(parse_targets(&json!(null)).unwrap().is_empty());
        // A legacy free-form object target is not a comparison matrix (and must not error).
        assert!(parse_targets(&json!({ "endpoint": "https://x" }))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parse_targets_valid_array_parses() {
        let t = parse_targets(&json!([
            { "provider": "openai", "model": "gpt-4o" },
            { "provider": "google", "model": "gemini", "label": "g" },
        ]))
        .unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].provider, "openai");
        assert_eq!(t[1].label.as_deref(), Some("g"));
    }

    #[test]
    fn parse_targets_malformed_array_is_hard_error() {
        // An array is unambiguously a matrix; a bad element must fail loudly, not silently degrade.
        assert!(parse_targets(&json!([{ "model": "no-provider" }])).is_err());
        assert!(parse_targets(&json!(["just-a-string"])).is_err());
    }

    #[test]
    fn attach_cases_signals_truncation_explicitly() {
        use super::{attach_cases, MAX_LOGGED_CASES};
        // A short list is complete, and says so.
        let mut r = json!({ "mode": "simple" });
        attach_cases(
            &mut r,
            "cases",
            vec![json!({ "case": 1 }), json!({ "case": 2 })],
        );
        assert_eq!(r["cases"].as_array().unwrap().len(), 2);
        assert_eq!(r["cases_total"], json!(2));
        assert_eq!(r["cases_logged"], json!(2));
        assert_eq!(r["cases_truncated"], json!(false));

        // A long one is clipped — and a consumer can tell, which is the whole point.
        let many: Vec<serde_json::Value> = (0..MAX_LOGGED_CASES + 50)
            .map(|i| json!({ "case": i }))
            .collect();
        let mut r = json!({ "mode": "compare" });
        attach_cases(&mut r, "cases", many);
        assert_eq!(r["cases"].as_array().unwrap().len(), MAX_LOGGED_CASES);
        assert_eq!(r["cases_total"], json!(MAX_LOGGED_CASES + 50));
        assert_eq!(r["cases_logged"], json!(MAX_LOGGED_CASES));
        assert_eq!(
            r["cases_truncated"],
            json!(true),
            "a clipped list must never look complete"
        );
        assert_eq!(
            r["cases"][0]["case"],
            json!(0),
            "the preview is the first k, in case order"
        );
    }

    #[test]
    fn add_price_warnings_only_when_present() {
        let mut r = json!({ "mode": "simple" });
        add_price_warnings(&mut r, &BTreeSet::new());
        assert!(r.get("price_warnings").is_none());
        let mut w = BTreeSet::new();
        w.insert("openai/gpt-4o".to_string());
        add_price_warnings(&mut r, &w);
        assert_eq!(r["price_warnings"], json!(["openai/gpt-4o"]));
        assert_eq!(join_csv(&w), "openai/gpt-4o");
    }
}
