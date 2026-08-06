//! `calibrate --compare-batch N` — measure what batching does to *your* rubric before trusting it.
//!
//! Batching is a methodology change: a judge that sees N cases at once may anchor on them, so a
//! batched score is not automatically the score the same case would get alone. Nobody can tell you in
//! the abstract whether that shift is negligible for your rubric — it depends on the rubric, the
//! judge model, and how similar your cases are to each other. So this measures it.
//!
//! The design is **paired**: the same items are judged both ways, so every difference is a method
//! difference rather than a sampling one, and the repo's existing paired statistics
//! ([`crate::stats::paired`]) apply directly. Three numbers decide it:
//!
//! - **mean |Δ| per case** — how much an individual verdict moves. This is what matters if you read
//!   case-level scores.
//! - **mean Δ with its paired p** — whether the *aggregate* moved. This is what matters for a gate,
//!   because a benchmark compares means. A large per-case scatter that cancels out is a very
//!   different problem from a systematic shift.
//! - **pass/fail flips** — how many cases crossed the rubric's threshold. A score that moves 0.02 is
//!   irrelevant unless it moved across the line.

use anyhow::{Context, Result};

use lighttrack_core::{ModelPriceRow, Rubric};
use lighttrack_engine::{run_rubric_batch, run_rubric_judge, BatchCase, EngineConfig};

use crate::stats::paired::{paired_deltas, paired_z};
use crate::util::{parallel_map, price_gen_cost};
use lighttrack_core::CalibrationItem;

/// One item judged both ways.
struct Paired {
    label: String,
    single: f64,
    batched: f64,
}

/// A scored item: its overall and what the call cost. `Result` because either method can fail on an
/// item without the comparison as a whole failing.
type Scored = Result<(f64, f64)>;

/// One batch's scored items, carrying the index each belongs to so results reassemble in item order.
type BatchScored = Vec<(usize, Scored)>;

/// What the comparison found. Returned (not just printed) so a caller can gate on it.
pub(crate) struct BatchComparison {
    pub(crate) n: usize,
    pub(crate) mean_abs_delta: f64,
    pub(crate) max_abs_delta: f64,
    pub(crate) mean_delta: f64,
    pub(crate) p: Option<f64>,
    pub(crate) flips: usize,
    pub(crate) single_mean: f64,
    pub(crate) batched_mean: f64,
    pub(crate) single_calls: usize,
    pub(crate) batched_calls: usize,
    pub(crate) single_cost: f64,
    pub(crate) batched_cost: f64,
}

/// Judge `items` singly and then in batches of `batch`, and report what changed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare(
    engine: &EngineConfig,
    jp: &str,
    jm: &str,
    rubric: &Rubric,
    items: &[CalibrationItem],
    batch: usize,
    samples: u32,
    jobs: usize,
    threshold: f64,
    prices: &[ModelPriceRow],
) -> Result<BatchComparison> {
    let batch = batch.max(2);
    println!(
        "comparing batch={batch} against single judging — {} item(s), judge={jp}/{jm}, rubric={}",
        items.len(),
        rubric.name
    );

    // --- reference: every item judged alone -------------------------------------------------
    println!("  judging singly ({} call(s))...", items.len());
    let single: Vec<Scored> = parallel_map(items.len(), jobs, |i| {
        let o = run_rubric_judge(
            engine,
            jp,
            jm,
            rubric,
            &items[i].input,
            items[i].expected.as_deref(),
            &items[i].output,
            samples,
            1,
        )
        .context("single judge failed")?;
        let c = o
            .cost_usd
            .unwrap_or_else(|| price_gen_cost(prices, jp, jm, o.input_tokens, o.output_tokens));
        Ok((o.overall, c))
    });

    // --- the method under test: the same items, batched ---------------------------------------
    // Clamp exactly as `bench --batch` does. A calibration that measured a batch size the benchmark
    // would never actually use would be worse than none: it would bless a method nobody runs.
    let by_output =
        crate::batch::cases_per_response(rubric, crate::batch::max_out_tokens_from_env());
    let effective = batch.min(by_output);
    if effective < batch {
        println!(
            "  batch reduced {batch} -> {effective}: {} LLM dimension(s) would push {batch} \
             verdicts past the response budget (this is what `bench --batch {batch}` would do too)",
            rubric.dimensions.iter().filter(|d| d.kind.is_llm()).count()
        );
    }
    let groups: Vec<Vec<usize>> = (0..items.len())
        .collect::<Vec<_>>()
        .chunks(effective)
        .map(<[usize]>::to_vec)
        .collect();
    println!("  judging batched ({} call(s))...", groups.len());
    let batched: Vec<BatchScored> = parallel_map(groups.len(), jobs, |g| {
        let group = &groups[g];
        let cases: Vec<BatchCase<'_>> = group
            .iter()
            .map(|&i| BatchCase {
                input: &items[i].input,
                expected: items[i].expected.as_deref(),
                output: &items[i].output,
            })
            .collect();
        match run_rubric_batch(engine, jp, jm, rubric, &cases, samples, 1) {
            Ok(per_case) => group
                .iter()
                .zip(per_case)
                .map(|(&i, r)| {
                    let v = r.map(|o| {
                        let c = o.cost_usd.unwrap_or_else(|| {
                            price_gen_cost(prices, jp, jm, o.input_tokens, o.output_tokens)
                        });
                        (o.overall, c)
                    });
                    (i, v.context("batched judge failed"))
                })
                .collect(),
            Err(e) => group
                .iter()
                .map(|&i| (i, Err(anyhow::anyhow!("batch failed: {e}"))))
                .collect(),
        }
    });

    // --- pair them up, dropping any item either method failed to score ------------------------
    let mut by_index: Vec<Option<(f64, f64)>> = (0..items.len()).map(|_| None).collect();
    let mut batch_err: Vec<Option<String>> = (0..items.len()).map(|_| None).collect();
    for (i, r) in batched.into_iter().flatten() {
        match r {
            Ok(v) => by_index[i] = Some(v),
            Err(e) => batch_err[i] = Some(e.to_string()),
        }
    }

    let mut pairs: Vec<Paired> = Vec::new();
    let (mut single_cost, mut batched_cost) = (0.0, 0.0);
    // A dropped item is evidence, not noise. Silently reporting a count hid a whole batch call
    // failing wholesale — which is itself a finding about batching at that size, and the single most
    // useful thing this command can tell an operator when it happens.
    let mut drops: Vec<String> = Vec::new();
    for (i, s) in single.into_iter().enumerate() {
        let label = items[i]
            .note
            .clone()
            .unwrap_or_else(|| format!("#{}", i + 1));
        match (s, by_index[i]) {
            (Ok((sv, sc)), Some((bv, bc))) => {
                single_cost += sc;
                batched_cost += bc;
                pairs.push(Paired {
                    label,
                    single: sv,
                    batched: bv,
                });
            }
            // An item only one method could score tells us nothing about the difference between
            // them, and averaging over a different set on each side would fake the comparison.
            (Err(e), _) => drops.push(format!("{label}: single judging failed — {e}")),
            (Ok(_), None) => drops.push(format!(
                "{label}: batched judging produced no verdict — {}",
                batch_err[i].as_deref().unwrap_or("no reason recorded")
            )),
        }
    }
    if !drops.is_empty() {
        println!(
            "\n  {} of {} item(s) dropped (scored by only one method):",
            drops.len(),
            items.len()
        );
        for d in &drops {
            println!("    {d}");
        }
        println!(
            "  A batch failing as a whole is a result about batch={batch}, not a glitch: it means \
             the response could not be produced or parsed at that size."
        );
    }
    if pairs.len() < 2 {
        anyhow::bail!(
            "only {} item(s) were scored by both methods — too few to compare",
            pairs.len()
        );
    }

    let singles: Vec<f64> = pairs.iter().map(|p| p.single).collect();
    let batcheds: Vec<f64> = pairs.iter().map(|p| p.batched).collect();
    let deltas = paired_deltas(&batcheds, &singles).unwrap_or_default();
    let (mean_delta, _z, p) =
        paired_z(&deltas).map_or((0.0, 0.0, None), |(m, z, p)| (m, z, Some(p)));

    let n = pairs.len();
    let mean = |xs: &[f64]| xs.iter().sum::<f64>() / xs.len() as f64;
    let cmp = BatchComparison {
        n,
        mean_abs_delta: mean(&deltas.iter().map(|d| d.abs()).collect::<Vec<_>>()),
        max_abs_delta: deltas.iter().fold(0.0_f64, |m, d| m.max(d.abs())),
        mean_delta,
        p,
        flips: pairs
            .iter()
            .filter(|p| (p.single >= threshold) != (p.batched >= threshold))
            .count(),
        single_mean: mean(&singles),
        batched_mean: mean(&batcheds),
        single_calls: items.len(),
        batched_calls: groups.len(),
        single_cost,
        batched_cost,
    };
    print_items(&pairs, threshold);
    print_report(&cmp, threshold, effective);
    Ok(cmp)
}

/// Per-item, so the *shape* of the difference is visible — a uniform lift, a few wild cases, or
/// leniency concentrated on the answers that should have scored worst. The aggregate cannot show
/// that, and the shape is what tells an operator whether they trust the method.
fn print_items(pairs: &[Paired], threshold: f64) {
    println!(
        "
  {:<12}  {:>7}  {:>7}  {:>8}  flip",
        "item", "single", "batch", "delta"
    );
    for p in pairs {
        let flipped = (p.single >= threshold) != (p.batched >= threshold);
        println!(
            "  {:<12}  {:>7.3}  {:>7.3}  {:>+8.3}  {}",
            p.label,
            p.single,
            p.batched,
            p.batched - p.single,
            if flipped { "FLIP" } else { "" }
        );
    }
}

fn print_report(c: &BatchComparison, threshold: f64, batch: usize) {
    println!("\n  batch={batch} vs single, over {} paired item(s)", c.n);
    println!(
        "  calls        {:>8} -> {:<8} ({:.1}x fewer)",
        c.single_calls,
        c.batched_calls,
        c.single_calls as f64 / c.batched_calls.max(1) as f64
    );
    println!(
        "  judge cost   ${:>7.4} -> ${:<7.4}",
        c.single_cost, c.batched_cost
    );
    println!(
        "  mean score   {:>8.4} -> {:<8.4}  (Δ {:+.4})",
        c.single_mean, c.batched_mean, c.mean_delta
    );
    println!(
        "  per-case |Δ| mean {:.4}, worst {:.4}",
        c.mean_abs_delta, c.max_abs_delta
    );
    println!(
        "  pass/fail flips at threshold {threshold:.2}: {} of {}",
        c.flips, c.n
    );
    match c.p {
        Some(p) => println!("  paired p on the mean shift: {p:.4}"),
        None => println!("  paired p: n/a (no spread to test)"),
    }

    // The verdict is deliberately conservative, and deliberately does NOT lead with the p-value.
    //
    // A golden set is small by nature, so the paired test is underpowered: a real, large shift can
    // sit at p ≈ 0.06 purely for want of items. Reading that as "the aggregate held" is exactly the
    // wrong inference — absence of significance is not evidence of absence, and a benchmark tool
    // that says "looks fine" because n was too small to prove otherwise is worse than useless.
    // So effect size decides, and the p-value only ever *adds* doubt.
    let flip_rate = c.flips as f64 / c.n as f64;
    let underpowered = c.n < 30;
    let systematic = c.p.is_some_and(|p| p < 0.05);
    let big_shift = c.mean_delta.abs() > 0.05;
    let scattered = c.mean_abs_delta > 0.10;

    println!();
    if systematic || big_shift || flip_rate > 0.25 {
        println!(
            "  VERDICT: batching at {batch} CHANGES this rubric's scores. Mean {:+.4}, \
             per-case |Δ| {:.4}, {} of {} cases crossed the pass/fail line.",
            c.mean_delta, c.mean_abs_delta, c.flips, c.n
        );
        println!(
            "  Do not batch this rubric, and do not compare a batched run to an unbatched \
             baseline. If you want the throughput, re-baseline and batch everything from then on \
             — and re-run this check first at a smaller batch size."
        );
        if !systematic && underpowered {
            println!(
                "  (The paired test did not reach p<0.05, but with only {} items it could not be \
                 expected to. The effect size is what condemns it, not the p-value.)",
                c.n
            );
        }
    } else if scattered || c.flips > 0 {
        println!(
            "  VERDICT: the mean is stable but individual verdicts move (per-case |Δ| {:.4}, \
             {} flip(s) of {}). Usable for tracking an aggregate over time; NOT usable if you act \
             on case-level pass/fail.",
            c.mean_abs_delta, c.flips, c.n
        );
    } else {
        println!(
            "  VERDICT: no detectable difference at batch={batch} for this rubric \
             (mean {:+.4}, per-case |Δ| {:.4}, no flips). Batching looks safe here.",
            c.mean_delta, c.mean_abs_delta
        );
        if underpowered {
            println!(
                "  Caveat: {} items is a small set. This says the effect is not large; it cannot \
                 say it is zero.",
                c.n
            );
        }
    }
    println!(
        "  Note: one rubric, one judge, {} item(s), batch={batch}. It does not license batching \
         elsewhere, and a rubric or judge change invalidates it.",
        c.n
    );
}
