//! Comparison mode: generate outputs from each target, judge them, compare quality × cost × latency.
//! Records per-dimension breakdown + agreement. With `gen_samples > 1` it generates several
//! candidates per case and averages their scores (generation self-consistency), so a single
//! lucky/unlucky output doesn't dominate — the judge is sampled separately via `samples`.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::Result;
use serde_json::{json, Map, Value};

use lighttrack_core::{
    BenchTarget, Benchmark, BenchmarkCase, BenchmarkRun, ModelPriceRow, Rubric, ScoreDetail,
    ScoreKind,
};
use lighttrack_engine::{parse_judge_spec, same_family, Determinism, EngineConfig};

use crate::bench::judge_output;
use crate::budget::{estimate_compare, Budget};
use crate::cli::Cli;
use crate::history::previous_case_scores;
use crate::http::{get, post};
use crate::provenance::{merge_details, weakest_reasoning};
use crate::runctl::RunControl;
use crate::stats::{
    annotate_significance, annotate_verdict, paired_deltas, stability, superiority, verdict,
    Summary,
};
use crate::targets::ResolvedTarget;
use crate::util::{
    add_price_warnings, aggregate_status, cost_or_book, join_csv, now_ts, parallel_map,
    percentiles, stamp_determinism,
};

/// One target's leaderboard row, in column order:
/// `(label, mean, pass_rate, gen_cost, judge_cost, p50_ms, errored, agreement)`.
type LeaderboardCells = (String, f64, f64, f64, f64, u64, u32, f64);

/// One `(target, case)` cell's independent result: the candidate scores/agreements plus this cell's
/// cost/latency/token contributions. Computed in parallel, then folded in case order so the per-target
/// leaderboard, posted scores, and printed log are byte-identical at any `--jobs`.
struct Cell {
    cand_scores: Vec<f64>,
    judge_agrees: Vec<f64>,
    cand_passes: u32,
    case_dim_sums: HashMap<String, f64>,
    /// One entry per judged candidate: the judge's full provenance for that candidate. Merged (not
    /// discarded) when the cell's mean score is posted.
    cand_details: Vec<ScoreDetail>,
    case_judge_cost: f64,
    gen_cost: f64,
    gen_tokens: u64,
    judge_cost: f64,
    judge_tokens: u64,
    latencies: Vec<u64>,
    /// Weakest reproducibility stamp across this cell's *generation* calls. `None` when nothing
    /// generated (the cell errored before its first candidate).
    gen_determinism: Option<Determinism>,
    /// Models with no price-book entry seen while pricing this cell (cost undercounted).
    price_warnings: BTreeSet<String>,
    /// First generation/judge error hit while sampling this cell (printed in the sequential fold).
    error_msg: Option<String>,
    /// True when the cell was never run because the run's dollar ceiling was already reached. A
    /// skipped cell is NOT an errored one: nothing failed, the operator's budget ran out — and the
    /// difference is what keeps a halted run from reading like a run that judged everything badly.
    skipped: bool,
    /// True when this cell was never run because the target's breaker was open (implies `skipped`).
    /// Reported apart from a budget skip: "we stopped paying" and "we stopped believing in this
    /// provider" are different facts about a missing row, and a leaderboard that renders them the
    /// same trades the product's own output for latency.
    filtered: bool,
    /// True when this cell was attempted *despite* an open breaker, because every target's breaker
    /// was open. Recorded, never silent: an operator watching traffic climb against a provider
    /// their dashboard shows as open needs the line that says the empty-set rule fired.
    breaker_override: bool,
}

/// How many consecutive generation failures open one target's breaker.
const OPEN_AFTER_FAILURES: u32 = 3;

/// One breaker per target in the benchmark matrix, used as a **filter over the candidate set**
/// rather than as admission control over a single call. The matrix's candidates are interchangeable
/// only in the sense that any of them can be measured next; the breaker's verdict is therefore an
/// input to "should this cell be dispatched?", not a permission.
///
/// Identity is the target's index in the caller's list and pruning never removes a member — the
/// filter answers per index. A filter that compacted the list would renumber every target after the
/// pruned one, and the next verdict would land on a healthy target that inherited a sick one's
/// streak.
pub(crate) struct TargetHealth {
    consecutive: Vec<AtomicU32>,
    threshold: u32,
}

/// The filter's verdict for one target.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Admit {
    /// Breaker closed — an ordinary attempt.
    Attempt,
    /// Breaker open, but so is every other target's: attempt anyway, and say so.
    Override,
    /// Breaker open while some other target still looks healthy: skip this cell.
    Filtered,
}

impl TargetHealth {
    pub(crate) fn new(n_targets: usize, threshold: u32) -> Self {
        TargetHealth {
            consecutive: (0..n_targets).map(|_| AtomicU32::new(0)).collect(),
            // A misconfigured `0` must not open every breaker on the first cell — the same reason
            // `crates/responder/src/breaker.rs:56-57` floors its permit count at one.
            threshold: threshold.max(1),
        }
    }

    fn is_open(&self, ti: usize) -> bool {
        self.consecutive
            .get(ti)
            .is_some_and(|c| c.load(Ordering::Relaxed) >= self.threshold)
    }

    /// Whether target `ti`'s next cell should be dispatched.
    ///
    /// **All open is not "no candidates".** A health hypothesis that indicts every target at once is
    /// the case where it is least likely to be exactly right and most expensive to obey: refusing on
    /// it turns a partial outage into a total one, and it also removes the only thing that could
    /// revise it, since the traffic that closes a breaker is traffic allowed through it. This is a
    /// set narrowed by *health*, so it fails open; a set narrowed by *permission* would fail closed.
    pub(crate) fn admit(&self, ti: usize) -> Admit {
        if !self.is_open(ti) {
            return Admit::Attempt;
        }
        if (0..self.consecutive.len()).all(|i| self.is_open(i)) {
            Admit::Override
        } else {
            Admit::Filtered
        }
    }

    /// Feed the breaker. Only **generation** failures count: a judge error or an unparseable verdict
    /// says nothing about whether this target is alive, and letting it trip the target's breaker
    /// would punish a healthy provider for the judge's bad hour.
    pub(crate) fn record(&self, ti: usize, generation_failed: bool) {
        if let Some(c) = self.consecutive.get(ti) {
            if generation_failed {
                c.fetch_add(1, Ordering::Relaxed);
            } else {
                // Closed by demonstrated success, not by the passage of time.
                c.store(0, Ordering::Relaxed);
            }
        }
    }
}

/// Generate `ng` candidates for one case from one target and judge each; pure (no printing/posting)
/// so it can run concurrently. A generation/judge error stops sampling this cell and is reported back
/// via `error_msg`; whatever candidates already scored are kept (matching the sequential behaviour).
#[allow(clippy::too_many_arguments)]
fn compute_cell(
    engine: &EngineConfig,
    rt: &ResolvedTarget,
    jp: &str,
    jm: &str,
    rubric: &Option<Rubric>,
    bench: &Benchmark,
    case: &BenchmarkCase,
    ng: u32,
    samples: u32,
    prices: &[ModelPriceRow],
    budget: &Budget,
    ctl: &RunControl,
    health: &TargetHealth,
    ti: usize,
) -> Cell {
    let t = &rt.target;
    let mut cell = Cell {
        cand_scores: Vec::new(),
        judge_agrees: Vec::new(),
        cand_passes: 0,
        case_dim_sums: HashMap::new(),
        cand_details: Vec::new(),
        case_judge_cost: 0.0,
        gen_cost: 0.0,
        gen_tokens: 0,
        judge_cost: 0.0,
        judge_tokens: 0,
        latencies: Vec::new(),
        gen_determinism: None,
        price_warnings: BTreeSet::new(),
        error_msg: None,
        skipped: false,
        filtered: false,
        breaker_override: false,
    };
    // Both stop conditions are checked HERE — at a case boundary, before the first paid call of this
    // cell: the operator's dollar ceiling, and an operator-requested cancellation. Neither ever
    // interrupts a call in flight; which cells get skipped depends on scheduling, that the run is
    // partial does not, and that is what's reported.
    if budget.exhausted() || ctl.cancelled() {
        if budget.exhausted() {
            budget.halt();
        }
        cell.skipped = true;
        return cell;
    }
    // The third stop condition, also at a case boundary and also before the first paid call: this
    // target's health. A ten-target, sixty-case run against a dead provider used to spend sixty
    // calls learning it once.
    match health.admit(ti) {
        Admit::Filtered => {
            cell.filtered = true;
            cell.skipped = true;
            return cell;
        }
        Admit::Override => cell.breaker_override = true,
        Admit::Attempt => {}
    }
    // Pin the candidate exactly as the judge is pinned — but only when one candidate per case was
    // asked for. With `--gen-samples > 1` the operator is deliberately drawing a distribution;
    // temperature 0 would collapse every draw onto the same output and silently delete the feature,
    // so we sample and stamp the run `sampled` rather than quietly claiming reproducibility.
    let pin = ng == 1;
    // Only this — the target's own call failing — is evidence about the target's health.
    let mut generation_failed = false;
    for _ in 0..ng {
        // The target decides how it generates: a model call with the RESOLVED prompt (not the
        // target's stored literal), or a POST to the operator's own endpoint.
        let gen = match rt.generate(engine, &case.input, case.expected.as_deref(), pin) {
            Ok(g) => g,
            Err(e) => {
                cell.error_msg = Some(format!("generation error — {e}"));
                generation_failed = true;
                break;
            }
        };
        // A pinned call reports what the provider actually honoured (`exact` only with a real seed);
        // an unpinned multi-draw is `sampled` regardless of what the provider could have offered.
        let stamp = if pin {
            gen.determinism
        } else {
            Determinism::Sampled
        };
        cell.gen_determinism = Some(match cell.gen_determinism {
            Some(prev) => prev.weakest(stamp),
            None => stamp,
        });
        let (gc, gpriced) = cost_or_book(
            gen.cost_usd,
            prices,
            &t.provider,
            &t.model,
            gen.input_tokens,
            gen.output_tokens,
        );
        if !gpriced {
            cell.price_warnings
                .insert(format!("{}/{}", t.provider, t.model));
        }
        cell.gen_cost += gc;
        cell.gen_tokens += gen.input_tokens.unwrap_or(0) + gen.output_tokens.unwrap_or(0);
        if let Some(l) = gen.latency_ms {
            cell.latencies.push(l);
        }
        let jr = match judge_output(
            engine,
            jp,
            jm,
            rubric,
            bench,
            case,
            &gen.output,
            samples,
            prices,
        ) {
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
        cell.cand_details.push(jr.detail);
        if jr.pass {
            cell.cand_passes += 1;
        }
        for (k, v) in &jr.dimensions {
            *cell.case_dim_sums.entry(k.clone()).or_insert(0.0) += v;
        }
    }
    // Charge the run's ledger with what this cell actually cost, including the spend of a cell that
    // errored part-way: those calls were paid for too.
    budget.spend(cell.gen_cost + cell.judge_cost);
    health.record(ti, generation_failed);
    cell
}

/// Whether a run stopped early, and why — the honest status a partial run carries instead of a
/// verdict it did not earn.
fn halt_status(cancelled: bool, skipped: u32, verdict_status: &'static str) -> &'static str {
    match (cancelled, skipped) {
        (true, _) => "cancelled",
        (false, s) if s > 0 => "partial",
        _ => verdict_status,
    }
}

/// One target's leaderboard row: (label, mean, pass_rate, gen_cost, judge_cost, p50_ms, errored, agreement).
/// Round to 3 decimals for compact report JSON.
fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Decide whether the leaderboard may name a **winner**, from `(label, mean, per-case scores)`.
///
/// A bare argmax over means is not a finding: two targets 0.01 apart with wide overlapping intervals
/// used to get a bold "Best mean" line. The top target is tested against the runner-up **paired**, on
/// the cases both were scored on, at α corrected across every pair a "best" claim implicitly chose
/// between (`m·(m−1)/2`, since the pair was picked *after* seeing the means). When the separation
/// isn't real the claim is downgraded to "highest mean, not significantly ahead" — a fact about the
/// sample, not about the models.
fn best_claim(per_target: &[(String, f64, Vec<f64>)]) -> Value {
    let mut ranked: Vec<&(String, f64, Vec<f64>)> = per_target
        .iter()
        .filter(|(_, _, cs)| !cs.is_empty())
        .collect();
    if ranked.is_empty() {
        return Value::Null;
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top = ranked[0];
    let n = per_target.len();
    let mut claim = json!({
        "label": top.0, "mean": r3(top.1), "significant": false,
        "correction": format!(
            "Bonferroni over {} target pair(s), family-wise α=0.05", (n * n.saturating_sub(1) / 2).max(1)
        ),
    });
    match ranked.get(1) {
        None => {
            claim["note"] = json!("only one target produced scores — nothing to be better than");
        }
        Some(second) => {
            claim["runner_up"] = json!(second.0);
            claim["runner_up_mean"] = json!(r3(second.1));
            match superiority(&top.2, &second.2, n) {
                Some((delta, p, significant)) => {
                    claim["mean_delta"] = json!(r3(delta));
                    claim["p_value"] = json!((p * 1e6).round() / 1e6);
                    claim["significant"] = json!(significant);
                    if !significant {
                        claim["note"] = json!(
                            "no significant difference from the runner-up at the corrected α — the \
                             ranking is not decidable at this sample size"
                        );
                    }
                }
                None => {
                    claim["note"] = json!(
                        "the top two targets were not scored on the same cases, so their gap cannot \
                         be tested"
                    );
                }
            }
        }
    }
    claim
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_compare(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    bench: &Benchmark,
    cases: &[BenchmarkCase],
    targets: &[ResolvedTarget],
    samples: u32,
    gen_samples: u32,
    pairwise: bool,
    jobs: usize,
    report_extra: Option<&Value>,
    ctl: &RunControl,
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
    let prices: Vec<ModelPriceRow> = crate::bench::fetch_prices(cli, http);

    // Cost pre-flight, matching pairwise's contract: print the call count and a dollar estimate
    // BEFORE the first paid call, and refuse to start a matrix that blows past `--max-cost`. A
    // compare run costs `targets × cases × gen_samples × (1 generation + judge_samples judge calls)`;
    // until now a fat-fingered `--gen-samples` was discovered only after it had been spent.
    let bench_targets: Vec<BenchTarget> = targets.iter().map(|r| r.target.clone()).collect();
    let estimate = estimate_compare(&prices, &bench_targets, cases.len(), ng, samples, &jp, &jm);
    println!("  {}", estimate.line());
    if !estimate.unpriced.is_empty() {
        println!(
            "  warning: no price book entry for {} — the estimate below excludes them and the \
             run's recorded cost will be undercounted",
            join_csv(&estimate.unpriced)
        );
    }
    if cli.max_cost > 0.0 && estimate.usd > cli.max_cost {
        println!(
            "  ABORT (compare): estimated ${:.2} exceeds --max-cost ${:.2}. Re-run with \
             --max-cost {:.2} to proceed, or reduce targets/cases/--gen-samples.",
            estimate.usd,
            cli.max_cost,
            // 10% headroom: the estimate is nominal, so a ceiling set to exactly it would halt the
            // very run the operator just approved.
            (estimate.usd * 1.1).max(0.01),
        );
        return Ok("aborted".to_string());
    }
    // The live ceiling. The pre-flight is an estimate over nominal token counts; this is the real
    // money, checked at every case boundary.
    let budget = Budget::new(cli.max_cost);

    // Prior runs of this benchmark, for the PAIRED per-case test: the same cases are judged in both
    // runs, so per-case deltas remove between-case variance and have far more power than comparing
    // this run's mean to a bare scalar. Best-effort — a benchmark with no readable history simply
    // falls back to the unpaired test (and the report says which one decided).
    let history: Vec<BenchmarkRun> =
        get(cli, http, &format!("/v1/benchmarks/{}/runs", bench.id)).unwrap_or_default();
    let dsv = report_extra
        .and_then(|e| e.get("dataset_version"))
        .and_then(Value::as_u64);
    // Every target is an independent hypothesis test against the same baseline, so the family-wise
    // error rate — not the per-test one — is what an operator actually experiences.
    let m = targets.len().max(1);

    let mut rows: Vec<LeaderboardCells> = Vec::new();
    // Per-target verdicts vs the benchmark baseline, rolled up into one honest run-level status below.
    let mut statuses: Vec<String> = Vec::new();
    // Per-target case scores, kept so the leaderboard's "best" claim can be tested — paired, on the
    // cases both targets were actually scored on — instead of asserted by argmax.
    let mut per_target: Vec<(String, f64, Vec<f64>)> = Vec::new();
    // Every unpriced model seen anywhere in the matrix, so the run-level output can say the totals
    // are undercounted instead of hiding it in each target's nested `price_warnings` array.
    let mut all_price_warnings: BTreeSet<String> = estimate.unpriced.clone();

    // Generate + judge the WHOLE (target, case) matrix with up to `jobs` concurrency. Targets used
    // to be an outer sequential loop with only cases parallelized inside, so wall-clock was
    // `n_targets × ceil(n_cases / jobs)` rounds instead of `ceil(n_targets · n_cases / jobs)` — the
    // job budget sat idle whenever a target had fewer cases than workers. Cells are folded below in
    // (target, case) order, so aggregation is byte-identical at any `--jobs`; see
    // `cell_matrix_is_order_independent`.
    let n_c = cases.len();
    let total_cells = targets.len() * n_c;
    // One breaker per target, consulted at each cell boundary. In-memory and run-scoped on purpose:
    // its whole evidence base is this run's cells, and carrying a streak across runs would open a
    // breaker on yesterday's incident.
    let health = TargetHealth::new(targets.len(), OPEN_AFTER_FAILURES);
    let cells: Vec<Cell> = parallel_map(total_cells, jobs, |idx| {
        let (ti, ci) = (idx / n_c.max(1), idx % n_c.max(1));
        let cell = compute_cell(
            engine,
            &targets[ti],
            &jp,
            &jm,
            &rubric,
            bench,
            &cases[ci],
            ng,
            samples,
            &prices,
            &budget,
            ctl,
            &health,
            ti,
        );
        // Live progress: the job's status line used to be written once at claim time and never
        // again, so a 500-case run looked identical at second 1 and minute 40.
        ctl.tick(total_cells);
        cell
    });
    let cancelled = ctl.cancelled();
    let mut cells = cells.into_iter();

    for rt in targets {
        let t = &rt.target;
        let label = t.display_label();
        println!("\n-- target {label} --");
        // One run per target, so the run id is minted per target — before judging, so every case
        // posted below is run-scoped even if this target's run post later fails.
        let run_id = lighttrack_core::new_id();
        let (mut overall_sum, mut passes, mut judged, mut gen_cost, mut judge_cost, mut errored) =
            (0.0_f64, 0u32, 0u32, 0.0_f64, 0.0_f64, 0u32);
        let mut latencies: Vec<u64> = Vec::new();
        let mut dim_sums: HashMap<String, f64> = HashMap::new();
        let mut agree_sum = 0.0_f64;
        let mut case_reports: Vec<Value> = Vec::new();
        let (mut gen_tokens, mut judge_tokens) = (0u64, 0u64);
        let mut price_warnings: BTreeSet<String> = BTreeSet::new();
        let mut case_scores: Vec<f64> = Vec::new();
        // Verdicts the API refused/couldn't take, and cases whose content imitated a judge-prompt
        // boundary. Both land in the run report instead of scrolling past on stderr.
        let (mut score_post_failures, mut injected) = (0u32, 0u32);
        // Weakest determinism stamp across this target's judged cells. `None` until something was
        // actually judged — a target with no verdicts claims nothing. Generation and judging are
        // tracked SEPARATELY: a pinned judge over a redrawn candidate is not a reproducible run, and
        // the old single stamp reported only the judge half.
        let mut target_determinism: Option<Determinism> = None;
        let mut target_gen_determinism: Option<Determinism> = None;

        // Self-preference (BENCHMARK_FRAMEWORK §3, "the four bias controls"): a judge from the same
        // lab as the target it grades tends to favour it. Documented as a control since the
        // framework was written and, until now, enforced by nothing. Warn and RECORD — never fail a
        // run: a same-family pairing is sometimes exactly what the operator wants to measure.
        // `family_provider` is the target's provider for a model and its HOST for an endpoint, so
        // an opaque service can never read as "the judge's own family" on a declared provider id.
        let self_preference = same_family(&jp, &jm, &t.family_provider(), &t.model);
        if self_preference {
            eprintln!(
                "  warning: SELF-PREFERENCE — judge {jp}/{jm} and target {}/{} are the same model \
                 family; this target's scores are biased upward. Judge on a different family (or \
                 use pairwise with a neutral judge) before publishing them.",
                t.provider, t.model
            );
        }

        // This target's slice of the matrix, folded in case order so cost/latency/agreement
        // aggregation is identical to the old sequential path.
        let target_cells: Vec<Cell> = cells.by_ref().take(n_c).collect();
        let mut skipped = 0u32;
        // Health-filtered cells, counted apart from budget/cancel skips, and cells attempted over an
        // open breaker under the empty-set rule.
        let (mut filtered, mut overrides) = (0u32, 0u32);

        for (i, cell) in target_cells.into_iter().enumerate() {
            if cell.breaker_override {
                overrides += 1;
            }
            if cell.filtered {
                filtered += 1;
                skipped += 1;
                continue;
            }
            if cell.skipped {
                skipped += 1;
                continue;
            }
            if let Some(msg) = &cell.error_msg {
                println!("  case {}: {msg}", i + 1);
            }
            price_warnings.extend(cell.price_warnings);
            if let Some(g) = cell.gen_determinism {
                target_gen_determinism =
                    Some(target_gen_determinism.map_or(g, |prev| prev.weakest(g)));
            }
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
                .map(|(k, v)| {
                    format!(
                        "{k}={}",
                        v.as_f64().map(|x| format!("{x:.2}")).unwrap_or_default()
                    )
                })
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
            // Per-case judge verdict → /v1/scores (queryable per case, not just the run aggregate),
            // carrying the merged provenance of every candidate judged for this cell rather than a
            // free-text "k=0.82 …" restatement of numbers already in `value`.
            let detail = merge_details(&cell.cand_details);
            if let Some(d) = detail.determinism.as_deref() {
                let stamp = if d == "exact" {
                    Determinism::Exact
                } else {
                    Determinism::BestEffort
                };
                target_determinism =
                    Some(target_determinism.map_or(stamp, |prev| prev.weakest(stamp)));
            }
            if detail.injection_suspected == Some(true) {
                injected += 1;
                eprintln!(
                    "  case {}: judged content imitated a prompt boundary (neutralized) — treat this \
                     score as attacker-adjacent",
                    i + 1
                );
            }
            let score = json!({
                "project_id": bench.project_id,
                "rubric": format!("{}:{label}#case{}", bench.name, i + 1),
                // This label embeds the case index, so it is unique per case — which is what
                // made every compare cell its own alert window and stopped any of them ever
                // accumulating. The kind is what lets the alert path roll them back up.
                "kind": ScoreKind::CompareCell.as_str(),
                "rubric_id": bench.rubric_id,
                "run_id": run_id, "case_index": i as u32 + 1,
                "value": r3(case_score), "max": 1.0, "pass": case_pass,
                "reasoning": weakest_reasoning(&detail),
                "detail": detail,
                "scored_by": format!("{jp}/{jm}"),
                "cost_usd": cell.case_judge_cost,
            });
            // Best-effort: a transient post failure must not abort a long comparison run — but it
            // must not vanish either. Log it and count it into the run report, so "the scores are
            // missing" is a recorded fact rather than something an operator has to infer.
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
        let mean_agree = if judged > 0 {
            agree_sum / judged as f64
        } else {
            1.0
        };
        let (p50, p95) = percentiles(&mut latencies);
        rows.push((
            label.clone(),
            mean,
            pass_rate,
            gen_cost,
            judge_cost,
            p50.unwrap_or(0),
            errored,
            mean_agree,
        ));

        // Per-target verdict vs the benchmark baseline: the absolute-floor CI test (now at the
        // family-wise-corrected critical z) composed with a paired per-case test against this
        // target's previous comparable run. Either firing means `regressed`, so the correction can
        // only trade a false alarm for a real detection — never disarm the gate.
        let summary = Summary::of(&case_scores);
        let prev = previous_case_scores(&history, &label, case_scores.len(), dsv);
        let deltas = prev.as_ref().and_then(|p| paired_deltas(&case_scores, p));
        let sig = verdict(
            if judged > 0 {
                bench.baseline_score
            } else {
                None
            },
            &summary,
            deltas.as_deref(),
            m,
        );
        let scalar_fallback = sig.scalar_fallback;
        // A budget-halted target judged only part of its cases, so its mean is a mean over whatever
        // the money reached — it must never be published under a verdict vocabulary that reads as a
        // completed run. `partial` is that honest status (the `--gate` contract treats it as
        // unverified, like `no_baseline`).
        let status = halt_status(cancelled, skipped, sig.status);
        if filtered > 0 {
            println!(
                "  HEALTH FILTER: {filtered} of {n_c} case(s) were never run — this target failed \
                 {OPEN_AFTER_FAILURES} consecutive generations, so its breaker opened. This row is \
                 PARTIAL because the provider was unavailable, which is itself a measurement — not \
                 because it scored badly."
            );
        }
        if overrides > 0 {
            println!(
                "  BREAKER OVERRIDE: {overrides} case(s) were attempted with EVERY target's breaker \
                 open. The empty-set rule fired: refusing to route when everything looks sick turns \
                 a partial outage into a total one, and only allowed traffic can ever close a breaker."
            );
        }
        // Budget/cancel skips are what the two lines below are about; a health-filtered cell has
        // already reported itself.
        let halted = skipped - filtered;
        if halted > 0 && cancelled {
            println!(
                "  CANCELLED: {halted} of {n_c} case(s) were never run — an operator stopped this \
                 run. Results below are PARTIAL."
            );
        } else if halted > 0 {
            println!(
                "  BUDGET HALT: {halted} of {n_c} case(s) were never run — the run's ${:.4} spend \
                 reached --max-cost ${:.2}. Results below are PARTIAL.",
                budget.spent_usd(),
                cli.max_cost,
            );
        }
        if let (Some(d), Some(p)) = (sig.mean_delta, sig.p_value) {
            println!(
                "  vs previous run (paired, n={}): mean Δ={d:+.3}, p={p:.4} (α={:.4} after {} \
                 -target correction)",
                summary.n, sig.alpha, m
            );
        }
        statuses.push(status.to_string());
        per_target.push((label.clone(), mean, case_scores.clone()));
        if !price_warnings.is_empty() {
            println!(
                "  warning: no price book entry for {} — cost undercounted",
                join_csv(&price_warnings)
            );
        }
        all_price_warnings.extend(price_warnings.iter().cloned());

        let dim_means: Map<String, Value> = dim_sums
            .iter()
            .map(|(k, s)| (k.clone(), json!(r3(s / judged.max(1) as f64))))
            .collect();
        let mut report = json!({
            "mode": "compare", "target": label, "provider": t.provider, "model": t.model,
            "prompt_label": t.label, "gen_cost_usd": gen_cost, "judge_cost_usd": judge_cost,
            "target_kind": if t.http_url().is_some() { "http" } else { "model" },
            "target_resolved_prompt_version": rt.resolved_version,
            "gen_tokens": gen_tokens, "judge_tokens": judge_tokens,
            "errored_cases": errored, "gen_samples": ng, "judge_samples": samples,
            "score_post_failures": score_post_failures,
            "injection_suspected_cases": injected,
            "self_preference": self_preference,
            "agreement": r3(mean_agree), "dimensions": Value::Object(dim_means),
            "verdict": status, "baseline": bench.baseline_score,
            // Spend control, recorded on the run so "why does this only have 40 cases?" is
            // answerable from the run alone.
            "partial": skipped > 0,
            "cancelled": cancelled,
            "budget_halted": skipped > filtered && !cancelled,
            "skipped_cases": skipped,
            // Rendered as distinctly as `partial` and `budget_halted`: a missing row because the
            // provider was down must not read like a missing row because the money ran out.
            "health_filtered_cases": filtered,
            "breaker_overrides": overrides,
            "cases_planned": n_c,
            "budget_limit_usd": budget.limit_usd(),
            "budget_spent_usd": budget.spent_usd(),
            "estimated_cost_usd": estimate.usd,
        });
        // The headline `determinism` is the weaker of generation and judging, with both halves
        // recorded beside it — a run whose candidates were resampled must not read as exact.
        stamp_determinism(&mut report, target_gen_determinism, target_determinism);
        // `cases` was unbounded — a big dataset wrote a report blob that grew with it. It is now a
        // bounded preview carrying its own truncation signal; the complete per-case record is the
        // run's scores (`GET /v1/scores?run=<id>`).
        crate::bench::attach_cases(&mut report, "cases", case_reports);
        annotate_significance(&mut report, &summary, scalar_fallback);
        annotate_verdict(&mut report, &sig);
        add_price_warnings(&mut report, &price_warnings);
        crate::bench::stamp_pins(&mut report, bench, report_extra);
        let run = json!({
            "id": run_id,
            "benchmark_id": bench.id, "n_cases": judged, "mean_score": mean, "pass_rate": pass_rate,
            "cost_usd": gen_cost + judge_cost, "status": status, "finished_at": now_ts(),
            "p50_latency_ms": p50, "p95_latency_ms": p95, "total_tokens": gen_tokens + judge_tokens,
            "report": report,
        });
        post(cli, http, "/v1/benchmark-runs", &run)?;
    }

    // One honest headline status for the whole comparison: regressed if any target regressed.
    let overall = aggregate_status(&statuses.iter().map(String::as_str).collect::<Vec<_>>());
    if cancelled {
        println!(
            "\nCANCELLED RUN: an operator stopped this comparison at a case boundary. The table \
             below is partial — it is not a finished comparison."
        );
    }
    if budget.halted() {
        println!(
            "\nPARTIAL RUN: the ${:.4} spend reached --max-cost ${:.2}; some cases were never run. \
             Treat the table below as a partial comparison, not a finished one.",
            budget.spent_usd(),
            cli.max_cost,
        );
    }
    if !all_price_warnings.is_empty() {
        println!(
            "\nwarning: no price book entry for {} — every $ figure below is a LOWER bound",
            join_csv(&all_price_warnings)
        );
    }
    if bench.baseline_score.is_some() {
        println!(
            "\ncompare verdict vs baseline {:.3}: {overall}",
            bench.baseline_score.unwrap_or(0.0)
        );
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
    let summary = json!({
        "n_cases": cases.len(), "targets": target_rows, "status": overall,
        "best": best_claim(&per_target),
        // Run-level spend facts, beside the leaderboard rather than buried per target.
        "budget_halted": budget.halted(),
        "cancelled": cancelled,
        "spend_usd": budget.spent_usd(),
        "budget_limit_usd": budget.limit_usd(),
        // Unpriced models make every $ figure above a LOWER bound. This is the aggregate view of
        // the per-target `price_warnings` arrays, which no reader of the table ever opened.
        "price_warnings": all_price_warnings.iter().cloned().collect::<Vec<_>>(),
    });
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
    use super::{r3, Admit, TargetHealth, OPEN_AFTER_FAILURES};
    use crate::util::parallel_map;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// The (target, case) → flat index mapping the run uses, exercised without live model calls.
    fn cell_at(idx: usize, n_c: usize) -> (usize, usize) {
        (idx / n_c, idx % n_c)
    }

    /// The compare matrix is now scheduled as ONE parallel_map over `n_targets × n_cases` instead of
    /// a sequential per-target loop. Aggregation must be unaffected: the same cells, in the same
    /// (target, case) order, folded into the same per-target totals at any `--jobs`. This is the
    /// compare-mode analogue of the k-sample judging order-independence test.
    #[test]
    fn cell_matrix_is_order_independent() {
        let (n_t, n_c) = (4, 7);
        // Deterministic stand-in for `compute_cell`: a cell's value depends only on its coordinates.
        let work = |ti: usize, ci: usize| (ti * 31 + ci * 7) as f64 / 10.0;

        let seq: Vec<f64> = parallel_map(n_t * n_c, 1, |i| {
            let (ti, ci) = cell_at(i, n_c);
            work(ti, ci)
        });
        let par: Vec<f64> = parallel_map(n_t * n_c, 16, |i| {
            let (ti, ci) = cell_at(i, n_c);
            work(ti, ci)
        });
        assert_eq!(
            seq, par,
            "the matrix schedule must be byte-identical at any --jobs"
        );

        // Each target's fold — the thing that becomes its leaderboard row — matches the old
        // outer-loop-per-target computation exactly.
        for ti in 0..n_t {
            let old: f64 = (0..n_c).map(|ci| work(ti, ci)).sum();
            let new: f64 = par[ti * n_c..(ti + 1) * n_c].iter().sum();
            assert!(approx(old, new), "target {ti}: {old} != {new}");
        }
        // …and the slice each target consumes is exactly its own cells, in case order.
        let mut it = par.iter();
        for ti in 0..n_t {
            let mine: Vec<f64> = it.by_ref().take(n_c).copied().collect();
            assert_eq!(mine, (0..n_c).map(|ci| work(ti, ci)).collect::<Vec<_>>());
        }
    }

    /// Cross-target parallelism inside the SAME `--jobs` budget: the old shape spent
    /// `n_targets × ceil(n_cases / jobs)` rounds because targets were an outer sequential loop, so a
    /// matrix with fewer cases than workers left most of the budget idle.
    #[test]
    fn cross_target_parallelism_cuts_wall_clock() {
        use std::time::{Duration, Instant};
        let (n_t, n_c, jobs, unit) = (3usize, 4usize, 8usize, Duration::from_millis(50));

        // Old: one parallel_map per target, targets sequential.
        let t0 = Instant::now();
        for _ in 0..n_t {
            parallel_map(n_c, jobs, |_| std::thread::sleep(unit));
        }
        let old = t0.elapsed();

        // New: one parallel_map over the whole matrix.
        let t1 = Instant::now();
        parallel_map(n_t * n_c, jobs, |_| std::thread::sleep(unit));
        let new = t1.elapsed();

        eprintln!("wall-clock {n_t}×{n_c} at --jobs {jobs}: per-target={old:?} matrix={new:?}");
        // 3 rounds of 50ms vs ceil(12/8) = 2 rounds. Only the *direction* is asserted: an absolute
        // millisecond bound would flake on a loaded machine, while the round count is structural —
        // identical total work, fewer scheduling rounds.
        assert!(
            new < old,
            "matrix scheduling must not be slower (old={old:?} new={new:?})"
        );
    }

    /// One run of the real dispatch shape — one `parallel_map` over `n_t × n_c` cells, folded in
    /// (target, case) order — with the health filter consulted at each cell boundary. `dead` names
    /// the targets whose generation call always fails. Returns per-target attempt counts and the
    /// filtered/override tallies.
    fn matrix(n_t: usize, n_c: usize, jobs: usize, dead: &[usize]) -> (Vec<u32>, u32, u32) {
        let health = TargetHealth::new(n_t, OPEN_AFTER_FAILURES);
        let attempts: Vec<AtomicU32> = (0..n_t).map(|_| AtomicU32::new(0)).collect();
        let (filtered, overrides) = (AtomicU32::new(0), AtomicU32::new(0));
        parallel_map(n_t * n_c, jobs, |idx| {
            let ti = idx / n_c;
            match health.admit(ti) {
                Admit::Filtered => {
                    filtered.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Admit::Override => {
                    overrides.fetch_add(1, Ordering::Relaxed);
                }
                Admit::Attempt => {}
            }
            attempts[ti].fetch_add(1, Ordering::Relaxed);
            health.record(ti, dead.contains(&ti));
        });
        (
            attempts.iter().map(|a| a.load(Ordering::Relaxed)).collect(),
            filtered.load(Ordering::Relaxed),
            overrides.load(Ordering::Relaxed),
        )
    }

    /// The measurable: a ten-target, sixty-case matrix in which target 7 fails every call used to
    /// spend sixty calls on it, learning the same fact sixty times. It now spends the breaker's
    /// threshold and stops — while every healthy target is measured on every case.
    #[test]
    fn health_filter_prunes_a_dead_target_from_the_matrix() {
        let (attempts, filtered, overrides) = matrix(10, 60, 1, &[7]);
        assert_eq!(
            attempts[7], OPEN_AFTER_FAILURES,
            "the dead target must cost the threshold, not the case count"
        );
        assert_eq!(filtered, 60 - OPEN_AFTER_FAILURES);
        assert_eq!(overrides, 0, "nine healthy targets means no empty-set rule");
        for (ti, n) in attempts.iter().enumerate() {
            if ti != 7 {
                assert_eq!(*n, 60, "target {ti} was healthy and must be fully measured");
            }
        }

        // Under real concurrency the streak is observed by several workers at once, so the bound is
        // threshold + in-flight rather than exactly threshold. What matters is the order of
        // magnitude: still nothing like sixty.
        let (par, _, _) = matrix(10, 60, 8, &[7]);
        assert!(
            par[7] <= OPEN_AFTER_FAILURES + 8,
            "dead target burned {} calls at --jobs 8 — nothing like the sixty it used to",
            par[7]
        );
    }

    /// The degenerate case, and the rule that is only ever discovered during an incident: when EVERY
    /// candidate's breaker is open the filter does not apply — the strategy runs over the full set
    /// and the call is attempted. Refusing here would convert a partial outage into a total one, and
    /// would remove the only traffic that could ever close a breaker. The paired assertion to
    /// `crates/responder/src/breaker.rs:56-57`, which floors its permits at one for the same reason.
    #[test]
    fn all_open_degrades_to_trying_not_to_refusing() {
        let health = TargetHealth::new(3, OPEN_AFTER_FAILURES);
        // One sick target among healthy ones is pruned.
        for _ in 0..OPEN_AFTER_FAILURES {
            health.record(0, true);
        }
        assert_eq!(health.admit(0), Admit::Filtered);
        assert_eq!(health.admit(1), Admit::Attempt);
        // Once the outage is total, every verdict flips to an attempt — recorded as an override,
        // never as an ordinary attempt and never as a refusal.
        for ti in 1..3 {
            for _ in 0..OPEN_AFTER_FAILURES {
                health.record(ti, true);
            }
        }
        for ti in 0..3 {
            assert_eq!(health.admit(ti), Admit::Override, "target {ti}");
        }
        // One target recovering re-arms the filter for the rest: the hypothesis is revisable, and
        // the traffic that revised it was traffic the override let through.
        health.record(2, false);
        assert_eq!(health.admit(2), Admit::Attempt);
        assert_eq!(health.admit(0), Admit::Filtered);
    }

    /// The same rule over the whole matrix: a total outage must never produce an empty run.
    #[test]
    fn a_total_outage_still_runs_cells() {
        let all: Vec<usize> = (0..10).collect();
        let (attempts, filtered, overrides) = matrix(10, 60, 1, &all);
        assert!(
            attempts.iter().all(|n| *n >= OPEN_AFTER_FAILURES),
            "no target may be starved of every attempt: {attempts:?}"
        );
        // Cells are dispatched target-major, so the last target is the one running once every
        // breaker is open — and it is attempted on all sixty, over the top of its own open breaker.
        assert_eq!(attempts[9], 60);
        assert_eq!(overrides, 60 - OPEN_AFTER_FAILURES);
        assert!(filtered > 0 && filtered < 600, "filtered={filtered}");
    }

    #[test]
    fn r3_rounds_to_three_decimals() {
        assert!(approx(r3(0.123456), 0.123));
        assert!(approx(r3(0.123654), 0.124)); // rounds half-away-from-zero at the 4th place
        assert!(approx(r3(1.0), 1.0));
        assert!(approx(r3(0.0), 0.0));
    }
}
