//! The cost–quality frontier, and the one further claim a compare run can honestly make: **the
//! cheapest target it could not tell apart from the best**.
//!
//! `best_claim` already answers "which target scored highest", tested. It has never answered the
//! question an operator actually spends money on — *how cheap can I go and still get away with it* —
//! which has a different answer whenever the cheap model sits inside the noise of the expensive one.
//! That is the single most common real finding a benchmark produces, and the leaderboard used to
//! leave it to the reader's eye across three independent columns.
//!
//! **The cost axis is generation cost per judged case.** Not total spend, and deliberately *not*
//! including judge cost: judging is benchmark overhead the operator never pays in production, so
//! folding it in would let a target that happens to be cheap to *grade* read as cheap to *run*. And
//! per case rather than per run, because targets judge different numbers of cases once errors and
//! health-filtering have had their say, which makes run totals incomparable.
//!
//! **Sufficiency is the existing corrected test read in reverse.** There is one statistics path in
//! this repo, and a second softer statistic invented for the *recommendation* — the strongest
//! sentence the tool prints — is precisely where it would do the most damage. A candidate is
//! sufficient iff `superiority(best, candidate, m)` comes back **not significant**: the run could
//! not show the best target ahead of it. That is an absence of evidence, so every recommendation
//! carries the case count and the corrected α that produced it.

use serde_json::{json, Value};

use super::cases::{CaseScore, Unpairable};
use super::normal::bonferroni_alpha;
use super::paired::{superiority, Superiority, ALPHA};
use super::{round3, EPS};

/// One target as the frontier sees it: the axes, plus the raw facts that decide whether it may be
/// placed on them at all.
#[derive(Debug, Clone)]
pub(crate) struct FrontierRow {
    pub(crate) label: String,
    pub(crate) mean: f64,
    /// This target's **generation** spend. The judge half of the run's cost is not here on purpose
    /// (see the module doc); a caller passing the total would silently change what is recommended.
    pub(crate) gen_cost_usd: f64,
    /// Cases this target was actually judged on — the denominator of the cost axis.
    pub(crate) judged_cases: u32,
    /// True when a generation had to fall back to the price book and found no entry, so
    /// `gen_cost_usd` is an undercount. Tracked apart from the run's `price_warnings`, which also
    /// carries the *judge's* unpriced model: an unpriced judge does not make a target's run cost
    /// unknown, and excluding a row for it would be a refusal nothing earned.
    pub(crate) gen_unpriced: bool,
    pub(crate) p50_ms: Option<u64>,
    pub(crate) p95_ms: Option<u64>,
    /// Per-case scores, each carrying the case it measured. Identified rather than bare, because the
    /// sufficiency test pairs them against the best target's — and two targets that errored on
    /// different cases arrive here the same length and one position out.
    pub(crate) scores: Vec<CaseScore>,
}

/// What the frontier needs from the run that is not a per-target fact.
pub(crate) struct FrontierInput<'a> {
    pub(crate) rows: &'a [FrontierRow],
    /// The run stopped early: a budget halt, an operator cancellation, or any skipped /
    /// health-filtered cell. Partial is contagious here — a run that scored a systematically later
    /// subset of its cases has not earned a recommendation, which is a stronger claim than a mean.
    pub(crate) partial: bool,
    /// Which of those fired, in the run's own vocabulary, so the refusal names it.
    pub(crate) partial_reason: &'static str,
}

/// A target with every axis known, so it can be compared on all of them.
struct Placed<'a> {
    row: &'a FrontierRow,
    cost_per_case: f64,
    p50: u64,
    p95: u64,
}

/// What the sufficiency test made of one frontier row. `IsBest` is deliberately *not* a test
/// result: the best target is the thing candidates are measured against, so measuring it against
/// itself is a tautology, and letting that tautology into the tally is how a run that separated
/// everything cleanly ended up reporting that it could separate nothing.
enum Verdict {
    /// This row *is* the reference — no test was run, and none could be.
    IsBest,
    /// The run could not show the best target ahead of it, at the corrected α. Carries the whole
    /// test, because the n it ran over is the intersection of two case sets and not either row's
    /// case count — and that n is the power the recommendation rests on.
    Sufficient(Superiority),
    /// The best target is significantly ahead of it: a real difference, not a cheaper option.
    Separated,
    /// Not pairable with the best target, with the reason: disjoint case sets, a single shared case,
    /// or a report that names one case twice are three different things to tell an operator.
    Untested(Unpairable),
}

/// Is `a` at least as good as `b` on every axis and strictly better on one? Quality is maximised;
/// cost per case and **both** latency percentiles are minimised. The tail is an axis of its own
/// because a target with a good median and a terrible p95 is exactly the trade-off a single-latency
/// surface hides, and it is the one an operator finds out about in production.
fn dominates(a: &Placed, b: &Placed) -> bool {
    let at_least_as_good = a.row.mean >= b.row.mean - EPS
        && a.cost_per_case <= b.cost_per_case + EPS
        && a.p50 <= b.p50
        && a.p95 <= b.p95;
    let strictly_better = a.row.mean > b.row.mean + EPS
        || a.cost_per_case < b.cost_per_case - EPS
        || a.p50 < b.p50
        || a.p95 < b.p95;
    at_least_as_good && strictly_better
}

/// Round a per-case cost without flattening it: these are routinely sub-cent, and `round3` would
/// turn every cheap target into `0.0` — the same "unknown reads as free" failure the exclusion rule
/// exists to prevent, arriving through the formatter instead.
fn r8(x: f64) -> f64 {
    (x * 1e8).round() / 1e8
}

/// A recommendation the run has not earned, with the reason in place of the name.
fn no_recommendation(note: String, alpha: f64, n_cases: usize, undecidable: Vec<Value>) -> Value {
    json!({
        "label": Value::Null, "note": note,
        "n_cases": n_cases, "alpha": (alpha * 1e6).round() / 1e6,
        "undecidable": undecidable,
    })
}

/// The frontier and the recommendation, as the two JSON objects the summary carries beside `best`.
fn analyze(input: &FrontierInput) -> (Value, Value) {
    let rows = input.rows;
    let n_targets = rows.len();
    let pairs = (n_targets * n_targets.saturating_sub(1) / 2).max(1);
    let alpha = bonferroni_alpha(ALPHA, pairs);

    // Exclusions first, by name and reason. An excluded target keeps its leaderboard row and stays
    // eligible to *be* the best — only its place on the cost/latency surface is unknown.
    let mut placed: Vec<Placed> = Vec::new();
    let mut excluded: Vec<Value> = Vec::new();
    for row in rows {
        let reason = if row.gen_unpriced {
            // The trap this whole module is written around: every HTTP provider adapter returns
            // `cost_usd: None` and cost comes from the DB price book, so an unpriced model is a
            // normal occurrence. Read as $0 it dominates the cost axis and becomes the
            // recommendation *precisely because nothing is known about it*.
            "no price-book entry for its model — generation cost unknown, and an unknown read as \
             $0 would dominate the cost axis"
        } else if row.judged_cases == 0 {
            "no case was judged, so there is no per-case cost to place it at"
        } else if row.p50_ms.is_none() || row.p95_ms.is_none() {
            "no latency recorded — an unknown read as 0ms would dominate both latency axes"
        } else {
            placed.push(Placed {
                row,
                cost_per_case: row.gen_cost_usd / row.judged_cases as f64,
                p50: row.p50_ms.unwrap_or_default(),
                p95: row.p95_ms.unwrap_or_default(),
            });
            continue;
        };
        excluded.push(json!({ "label": row.label, "reason": reason }));
    }

    let mut front: Vec<&Placed> = placed
        .iter()
        .filter(|a| !placed.iter().any(|b| dominates(b, a)))
        .collect();
    // Cheapest first — the walk order — with deterministic tie-breaks so two runs of the same matrix
    // never recommend different rows out of hash order.
    front.sort_by(|a, b| {
        a.cost_per_case
            .total_cmp(&b.cost_per_case)
            .then(b.row.mean.total_cmp(&a.row.mean))
            .then(a.row.label.cmp(&b.row.label))
    });
    let frontier = json!({
        "axes": "quality ↑, generation cost per judged case ↓, p50 ↓, p95 ↓",
        "non_dominated": front.iter().map(|p| json!(p.row.label)).collect::<Vec<_>>(),
        "excluded": excluded,
    });

    // The reference target, picked by the same rule as `best_claim` (first of the highest means
    // among targets that produced scores) so the two sentences on one leaderboard can never name
    // different bests.
    let mut best: Option<&FrontierRow> = None;
    for row in rows.iter().filter(|r| !r.scores.is_empty()) {
        if best.is_none_or(|b| row.mean > b.mean) {
            best = Some(row);
        }
    }
    let Some(best) = best else {
        let note = "no target produced any scores, so there is nothing to be cheaper than";
        return (frontier, no_recommendation(note.into(), alpha, 0, vec![]));
    };
    let n_cases = best.scores.len();
    if n_targets < 2 {
        // Mirrors `best_claim`'s single-target note: one row is not a comparison.
        let note = "only one target ran — there is nothing to be cheaper than";
        return (
            frontier,
            no_recommendation(note.into(), alpha, n_cases, vec![]),
        );
    }
    if input.partial {
        let note = format!(
            "the run was {} and scored only part of its cases — the sample is systematically \
             incomplete, and a recommendation is a stronger claim than a mean",
            input.partial_reason
        );
        return (frontier, no_recommendation(note, alpha, n_cases, vec![]));
    }

    // One verdict per frontier row, in cost order. **The best target is never its own candidate.**
    // `superiority(best, best)` is all-zero deltas — p = 1, "not significant" — which is not
    // evidence about anything; counting it as a test made the power caveat fire hardest on the
    // *strongest* runs there are. When the best target dominates every other row it is the only row
    // left on the frontier, so the self-comparison used to be the only "test" that ran, and the run
    // announced that it could distinguish nothing — while the rows it obviously separated had been
    // removed by domination before the walk ever saw them.
    let verdicts: Vec<Verdict> = front
        .iter()
        .map(|c| {
            if std::ptr::eq(c.row, best) {
                return Verdict::IsBest;
            }
            match superiority(&best.scores, &c.row.scores, n_targets) {
                // Not "sufficient" and not "insufficient" — untested, and the reason is kept.
                Err(e) => Verdict::Untested(e),
                Ok(s) if s.significant => Verdict::Separated,
                Ok(s) => Verdict::Sufficient(s),
            }
        })
        .collect();
    // Reported rather than skipped: a candidate silently dropped from the walk reads as one that
    // was considered and passed.
    let undecidable: Vec<Value> = front
        .iter()
        .zip(&verdicts)
        .filter_map(|(c, v)| match v {
            Verdict::Untested(e) => Some(json!({
                "label": c.row.label,
                "reason": format!("cannot be paired with the best target — {}", e.reason()),
            })),
            _ => None,
        })
        .collect();
    let tested = verdicts
        .iter()
        .filter(|v| matches!(v, Verdict::Sufficient(_) | Verdict::Separated))
        .count();
    let separated = verdicts
        .iter()
        .filter(|v| matches!(v, Verdict::Separated))
        .count();

    // The walk itself: cheapest row first, stopping at the first that ends it — a genuine candidate
    // the test cannot separate, or the best target's own row, which nothing cheaper qualified ahead of.
    let Some((cand, verdict)) = front
        .iter()
        .zip(&verdicts)
        .find(|(_, v)| matches!(v, Verdict::Sufficient(_) | Verdict::IsBest))
    else {
        let note = if front.is_empty() {
            "no target could be placed on the cost–quality frontier at all — every row was \
             excluded, for the reasons listed beside it"
                .to_string()
        } else {
            format!(
                "no target on the frontier could be shown indistinguishable from {} — every priced \
                 candidate was either significantly worse or could not be paired with it",
                best.label
            )
        };
        return (
            frontier,
            no_recommendation(note, alpha, n_cases, undecidable),
        );
    };
    // "Not significantly worse" is an absence of evidence: at a small case count *everything* is
    // indistinguishable from everything, and this sentence would confidently name the cheapest row
    // in the matrix. So the count and the surviving α travel with the claim — but the caveat is only
    // a power statement when there were genuine candidates to have power *over*.
    let all_indistinguishable = (tested > 0).then_some(separated == 0);
    // The n behind *this* claim is the cases the recommended row and the best target were BOTH
    // judged on — not the best target's own case count, which is the same number only when nothing
    // errored. A recommendation rests on an absence of evidence, so overstating the sample that
    // produced the absence is the one direction this must never quietly go.
    let (tested_n, dropped) = match verdict {
        Verdict::Sufficient(s) => (s.retained, s.dropped),
        _ => (n_cases, 0),
    };
    let note = match (verdict, all_indistinguishable) {
        // The clean answer, and a common one: nothing cheaper on the surface, so there is no
        // trade-off left to make. Said plainly, because an operator who reads a bare "nothing
        // found" concludes the tool learned nothing, when it learned the best result available.
        (Verdict::IsBest, _) => format!(
            "{} is also the cheapest row on the frontier, so nothing is given up by choosing it \
             and there is no cheaper target to trade quality against",
            best.label
        ),
        (_, Some(true)) => format!(
            "every candidate on the frontier was indistinguishable from {} at n={tested_n} — that \
             is a fact about this run's power, not a finding about the models",
            best.label
        ),
        _ => format!(
            "the run could not show {} ahead of it at the corrected α",
            best.label
        ),
    };
    // A subset pairing is a valid test over fewer cases, so it recommends — and says how much of the
    // run it left out. A target that errors on the hard cases leaves an easier intersection, which
    // makes "indistinguishable from the best" generalise less than the bare sentence suggests.
    let note = if dropped > 0 {
        format!(
            "{note}. Paired on the {tested_n} case(s) both were judged on; {dropped} case(s) were \
             judged by only one of them and are not in this test"
        )
    } else {
        note
    };
    let mut rec = json!({
        "label": cand.row.label,
        "best": best.label,
        "mean": round3(cand.row.mean),
        "best_mean": round3(best.mean),
        "cost_per_case_usd": r8(cand.cost_per_case),
        "n_cases": tested_n,
        // How many of the union of the two case sets the pairing had to leave out. 0 on a clean run,
        // and on the best target's own row, where no test ran at all.
        "cases_dropped": dropped,
        "best_judged_cases": n_cases,
        "alpha": (alpha * 1e6).round() / 1e6,
        "candidates_tested": tested,
        // `null`, not `false`, when nothing was a genuine candidate: with none to test, "were they
        // all indistinguishable?" has no answer rather than the answer "no".
        "all_candidates_indistinguishable": all_indistinguishable,
        "correction": format!("Bonferroni over {pairs} target pair(s), family-wise α={ALPHA}"),
        "undecidable": undecidable,
        "note": note,
    });
    // Absent on the best's own row: there was no test, and a p of 1.0 from comparing a target with
    // itself would read as a measured result.
    if let Verdict::Sufficient(s) = verdict {
        rec["p_value"] = json!((s.p * 1e6).round() / 1e6);
    }
    (frontier, rec)
}

/// Layer `frontier` and `recommendation` onto a compare run's summary, beside `best`. Additive JSON
/// — a stored summary from before this existed simply lacks both keys, and the render layer keeps
/// exactly the table it always had for one.
pub(crate) fn annotate_frontier(summary: &mut Value, input: &FrontierInput) {
    let (frontier, rec) = analyze(input);
    if let Some(obj) = summary.as_object_mut() {
        obj.insert("frontier".into(), frontier);
        obj.insert("recommendation".into(), rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row with everything known: quality, per-case cost (as a total over `n` cases), both
    /// latencies, and per-case scores over cases 1..n (the shape a target with no errored cell has).
    fn row(label: &str, scores: &[f64], cost_per_case: f64, p50: u64, p95: u64) -> FrontierRow {
        let cases: Vec<u32> = (1..=scores.len() as u32).collect();
        at_cases(label, &cases, scores, cost_per_case, p50, p95)
    }

    /// The same row, over an explicit set of case ids — for the targets that errored on some.
    fn at_cases(
        label: &str,
        cases: &[u32],
        scores: &[f64],
        cost_per_case: f64,
        p50: u64,
        p95: u64,
    ) -> FrontierRow {
        assert_eq!(cases.len(), scores.len());
        let n = scores.len().max(1) as u32;
        FrontierRow {
            label: label.into(),
            mean: scores.iter().sum::<f64>() / scores.len().max(1) as f64,
            gen_cost_usd: cost_per_case * n as f64,
            judged_cases: n,
            gen_unpriced: false,
            p50_ms: Some(p50),
            p95_ms: Some(p95),
            scores: cases
                .iter()
                .zip(scores)
                .map(|(c, s)| CaseScore::new(*c, *s))
                .collect(),
        }
    }

    fn run(rows: &[FrontierRow]) -> (Value, Value) {
        analyze(&FrontierInput {
            rows,
            partial: false,
            partial_reason: "",
        })
    }

    fn labels(v: &Value, key: &str) -> Vec<String> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| x["label"].as_str().unwrap_or("").to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A hand-built matrix whose answer is checkable by eye: `mid` is worse than `good` on quality
    /// AND dearer AND slower on both percentiles, so it is dominated and cannot appear. `cheap` is
    /// worst on quality but cheapest and fastest; `good` is best on quality. Both survive.
    #[test]
    fn a_dominated_target_never_reaches_the_frontier() {
        let (f, _) = run(&[
            row("cheap", &[0.60, 0.62, 0.58, 0.60], 0.001, 100, 150),
            row("mid", &[0.70, 0.71, 0.69, 0.70], 0.050, 500, 900),
            row("good", &[0.90, 0.91, 0.89, 0.90], 0.040, 400, 800),
        ]);
        assert_eq!(labels(&f, "non_dominated"), vec!["cheap", "good"]);
        assert!(
            labels(&f, "excluded").is_empty(),
            "nothing here is unpriced: {f}"
        );
    }

    /// The whole point of the feature: the recommendation is NOT the best-scoring target. `cheap`
    /// is 40× cheaper and its per-case gap to `best` flaps in sign, so the corrected test cannot
    /// separate them — and that is the row an operator should actually run.
    #[test]
    fn the_recommendation_is_the_cheapest_target_the_run_cannot_separate() {
        let cheap = [0.80, 0.71, 0.90, 0.61, 0.84, 0.75];
        let best = [0.79, 0.72, 0.88, 0.63, 0.85, 0.77];
        let (_, r) = run(&[
            row("cheap", &cheap, 0.001, 100, 150),
            row("dear", &best, 0.040, 400, 800),
        ]);
        assert_eq!(r["label"], json!("cheap"));
        assert_eq!(r["best"], json!("dear"), "the reference is the best mean");
        assert!(
            r["best_mean"].as_f64() > r["mean"].as_f64(),
            "the recommended row is NOT the top scorer: {r}"
        );
        // Power is disclosed on the claim itself, never left to the reader.
        assert_eq!(r["n_cases"], json!(6));
        assert_eq!(r["alpha"], json!(0.05), "one pair ⇒ α is uncorrected");
        assert!(r["p_value"].as_f64().unwrap_or(0.0) > 0.05);
    }

    /// …and the other half of the same rule: a cheapest target the run CAN separate is refused.
    /// `awful` is 0.30 below on every single case, so the paired test has overwhelming evidence.
    #[test]
    fn a_cheapest_target_the_run_can_separate_is_not_recommended() {
        let best = [0.90, 0.85, 0.92, 0.88, 0.91, 0.87];
        let awful: Vec<f64> = best.iter().map(|x| x - 0.30).collect();
        let (_, r) = run(&[
            row("awful", &awful, 0.001, 100, 150),
            row("dear", &best, 0.040, 400, 800),
        ]);
        assert_eq!(
            r["label"],
            json!("dear"),
            "a separated target must not be recommended: {r}"
        );
        assert_eq!(r["all_candidates_indistinguishable"], json!(false));
    }

    /// The dangerous one. The unpriced target has the BEST quality and a cost of nothing, which is
    /// exactly the shape that would dominate the cost axis and be recommended on the strength of
    /// what is not known about it. It must be excluded by name, with the reason.
    #[test]
    fn an_unpriced_target_is_excluded_by_name_never_priced_at_zero() {
        let top = [0.95, 0.94, 0.96, 0.93, 0.95, 0.94];
        let mut mystery = row("mystery", &top, 0.0, 10, 20);
        mystery.gen_unpriced = true;
        // Deliberately within noise of `mystery`, so a recommendation IS earned — the test then
        // discriminates "excluded" from "there was nothing to recommend anyway".
        let ok = [0.94, 0.95, 0.95, 0.94, 0.94, 0.95];
        let (f, r) = run(&[mystery, row("priced", &ok, 0.040, 400, 800)]);
        assert_eq!(labels(&f, "non_dominated"), vec!["priced"]);
        assert_eq!(labels(&f, "excluded"), vec!["mystery"]);
        assert!(
            f["excluded"][0]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("price-book"),
            "the reason travels with the name: {f}"
        );
        assert_ne!(
            r["label"],
            json!("mystery"),
            "an unknown cost must never become the recommendation: {r}"
        );
        // It is still eligible to be the *reference*: its quality is measured even where its cost
        // is not, and the run can only be separated from what it actually scored highest.
        assert_eq!(r["best"], json!("mystery"));
    }

    #[test]
    fn a_partial_run_recommends_nothing_and_says_why() {
        let a = [0.80, 0.71, 0.90, 0.61, 0.84, 0.75];
        let b = [0.79, 0.72, 0.88, 0.63, 0.85, 0.77];
        let rows = [
            row("cheap", &a, 0.001, 100, 150),
            row("dear", &b, 0.04, 4, 8),
        ];
        let (_, r) = analyze(&FrontierInput {
            rows: &rows,
            partial: true,
            partial_reason: "halted at its spend ceiling",
        });
        assert_eq!(r["label"], Value::Null);
        let note = r["note"].as_str().unwrap_or_default();
        assert!(
            note.contains("halted at its spend ceiling") && note.contains("stronger claim"),
            "the refusal names which stop condition fired: {note}"
        );
        // …and the same matrix, complete, does recommend — otherwise this test would pass on code
        // that never recommends anything.
        assert_eq!(run(&rows).1["label"], json!("cheap"));
    }

    #[test]
    fn a_single_target_run_has_nothing_to_be_cheaper_than() {
        let (_, r) = run(&[row("solo", &[0.8, 0.7, 0.9, 0.75], 0.01, 100, 200)]);
        assert_eq!(r["label"], Value::Null);
        assert!(r["note"]
            .as_str()
            .unwrap_or_default()
            .contains("nothing to be cheaper than"));
    }

    /// A candidate that shares no case with the best cannot be paired with it, so it is neither
    /// accepted nor quietly dropped: it is reported untested, and the walk continues past it.
    ///
    /// *(This test used to make `short` unpairable by giving it a shorter **vector** — two scores
    /// against six. Under by-case alignment that is cases 1 and 2 of the same dataset and pairs
    /// perfectly well, which is the whole point of the change; the unpairable case is now a genuine
    /// difference of case identity. It pins something the old version could not: that "unpairable"
    /// is decided by which cases were judged, not by how many.)*
    #[test]
    fn an_unpairable_candidate_is_reported_untested() {
        let near = [0.79, 0.72, 0.88, 0.63, 0.85, 0.77];
        let dear = [0.80, 0.71, 0.90, 0.61, 0.84, 0.75];
        // `stray` scored only cases 7 and 8 — no case in common with the six-case rows — so it is
        // cheapest and fastest, on the frontier, and cannot be paired with anything.
        let (_, r) = run(&[
            at_cases("stray", &[7, 8], &[0.50, 0.52], 0.0005, 50, 60),
            row("cheap", &near, 0.001, 100, 150),
            row("dear", &dear, 0.040, 400, 800),
        ]);
        assert_eq!(labels(&r, "undecidable"), vec!["stray"]);
        let reason = r["undecidable"][0]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("untested") && reason.contains("no case was judged in both"),
            "the refusal names WHICH refusal it is: {reason}"
        );
        assert_eq!(
            r["label"],
            json!("cheap"),
            "the walk steps past an untested row rather than accepting it: {r}"
        );
    }

    /// **The partial-overlap cell, at the level that spends money on the answer.** `patchy` errored
    /// on cases 5 and 6 — where the best target happens to be strong — so a naive positional pairing
    /// would difference its case-1..4 scores against the best's case-3..6 scores. It is neither
    /// refused nor paired blind: it is tested over the four cases both judged, recommended on that
    /// evidence, and the recommendation carries the REDUCED n and the drop count.
    #[test]
    fn a_partial_overlap_is_tested_over_the_intersection_with_its_n_disclosed() {
        let best = [0.70, 0.72, 0.71, 0.73, 0.95, 0.96];
        let patchy = [0.71, 0.71, 0.72, 0.72];
        let (_, r) = run(&[
            at_cases("patchy", &[1, 2, 3, 4], &patchy, 0.001, 100, 150),
            row("dear", &best, 0.040, 400, 800),
        ]);
        assert_eq!(r["label"], json!("patchy"), "{r}");
        assert_eq!(r["best"], json!("dear"));
        assert_eq!(
            r["n_cases"],
            json!(4),
            "the power disclosure is the PAIRED n, not the best target's six: {r}"
        );
        assert_eq!(r["cases_dropped"], json!(2), "{r}");
        assert_eq!(r["best_judged_cases"], json!(6));
        let note = r["note"].as_str().unwrap_or_default();
        assert!(
            note.contains("4 case(s) both were judged on") && note.contains("2 case(s)"),
            "the subset is disclosed in the sentence a reader actually sees: {note}"
        );
        assert!(r["p_value"].is_number(), "a real test ran: {r}");
        // A complete run over the same rows keeps a clean sentence — otherwise this test would pass
        // on code that always claims a subset.
        let (_, clean) = run(&[
            row("patchy", &best, 0.001, 100, 150),
            row("dear", &best, 0.040, 400, 800),
        ]);
        assert_eq!(clean["cases_dropped"], json!(0));
        assert!(!clean["note"]
            .as_str()
            .unwrap_or_default()
            .contains("judged by only one"));
    }

    /// Two rows that share exactly one case are not "the same cases" and not "different cases" —
    /// they are untestable, and the refusal says which of the two it is rather than collapsing both
    /// into one anonymous absence.
    #[test]
    fn a_single_shared_case_is_its_own_refusal() {
        let best = [0.90, 0.85, 0.92, 0.88, 0.91, 0.87];
        let (_, r) = run(&[
            at_cases("touching", &[6, 9], &[0.50, 0.52], 0.0005, 50, 60),
            row("dear", &best, 0.040, 400, 800),
        ]);
        assert_eq!(labels(&r, "undecidable"), vec!["touching"]);
        assert!(
            r["undecidable"][0]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("only 1 case was judged in both"),
            "a one-case overlap is not the same fact as no overlap: {r}"
        );
    }

    /// Three targets that are all within noise of each other: the recommendation is real, but the
    /// run could not distinguish *anything*, and that is a fact about the run rather than a finding
    /// about the models. Said out loud, with n and α.
    #[test]
    fn a_run_that_can_separate_nothing_says_so() {
        let a = [0.80, 0.71, 0.90, 0.61, 0.84, 0.75];
        let b = [0.79, 0.72, 0.88, 0.63, 0.85, 0.77];
        let c = [0.81, 0.70, 0.89, 0.62, 0.83, 0.76];
        let (_, r) = run(&[
            row("c1", &a, 0.001, 100, 150),
            row("c2", &b, 0.002, 110, 160),
            // Cheapest but slowest, so it is on the surface rather than dominated: TWO genuine
            // candidates get tested here, which is what separates this case from the two-target one.
            row("c3", &c, 0.0005, 300, 400),
        ]);
        assert_eq!(r["all_candidates_indistinguishable"], json!(true));
        assert_eq!(
            r["candidates_tested"],
            json!(2),
            "the two rows that are NOT the reference: {r}"
        );
        assert!(r["note"]
            .as_str()
            .unwrap_or_default()
            .contains("fact about this run's power"));
        assert!(
            r["p_value"].is_number(),
            "a genuine candidate was actually tested: {r}"
        );
        // Three targets ⇒ 3 implicit pairs ⇒ α = 0.05/3, disclosed on the claim.
        assert_eq!(r["alpha"], json!(0.016667));
        assert!(r["correction"]
            .as_str()
            .unwrap_or_default()
            .contains("Bonferroni over 3 target pair(s)"));
    }

    /// **The strongest run there is, and the one the power caveat used to libel.** When the best
    /// target is also the cheapest and fastest it dominates everything, so it is the *only* row left
    /// on the frontier — and the rows it obviously separated were removed by domination before the
    /// walk saw them. Comparing it with itself is a tautology (p = 1, "not significant"), and
    /// counting that as a test made the run announce that it could distinguish nothing. It must
    /// instead say the clean thing: nothing is given up by choosing the best.
    #[test]
    fn a_best_that_dominates_everything_is_recommended_with_no_trade_off_to_make() {
        let (f, r) = run(&[
            row(
                "best-and-cheapest",
                &[0.90, 0.92, 0.91, 0.93, 0.90, 0.92],
                0.001,
                100,
                200,
            ),
            row(
                "dear-and-worse",
                &[0.60, 0.62, 0.61, 0.63, 0.60, 0.62],
                0.010,
                300,
                900,
            ),
        ]);
        assert_eq!(labels(&f, "non_dominated"), vec!["best-and-cheapest"]);
        assert_eq!(r["label"], json!("best-and-cheapest"));
        assert_eq!(r["best"], json!("best-and-cheapest"));
        assert_eq!(
            r["candidates_tested"],
            json!(0),
            "a target is never its own candidate: {r}"
        );
        assert_eq!(
            r["all_candidates_indistinguishable"],
            Value::Null,
            "with no candidates the question has no answer, and certainly not `true`: {r}"
        );
        assert!(
            r["p_value"].is_null(),
            "a self-comparison is not a measured p: {r}"
        );
        let note = r["note"].as_str().unwrap_or_default();
        assert!(
            note.contains("nothing is given up by choosing it"),
            "the clean answer is stated, not left as a bare absence: {note}"
        );
        assert!(
            !note.contains("power"),
            "this run had ample power — it separated the other row by domination: {note}"
        );
    }

    /// The other side of the same rule, so the two cases cannot be satisfied by one behaviour: a
    /// GENUINE candidate that the run cannot separate is still recommended over the best, and still
    /// carries the real power caveat, because there really was a test that came back empty.
    #[test]
    fn a_genuine_indistinguishable_candidate_still_beats_the_best_on_cost() {
        let best = [0.80, 0.71, 0.90, 0.61, 0.84, 0.75];
        let near = [0.79, 0.72, 0.88, 0.63, 0.85, 0.77]; // marginally the higher mean
        let (_, r) = run(&[
            row("cheap", &best, 0.001, 100, 150),
            row("dear", &near, 0.040, 400, 800),
        ]);
        assert_eq!(r["label"], json!("cheap"));
        assert_eq!(r["best"], json!("dear"));
        assert_eq!(r["candidates_tested"], json!(1));
        assert_eq!(r["all_candidates_indistinguishable"], json!(true));
        assert!(r["p_value"].is_number(), "a real test ran: {r}");
        assert!(r["note"]
            .as_str()
            .unwrap_or_default()
            .contains("fact about this run's power"));
    }

    /// A tail-only difference is a real trade-off: `spiky` wins on p50 and loses badly on p95, so
    /// neither row dominates the other and both stay on the surface. Dropping p95 would collapse
    /// this to a single winner and hide the tail an operator meets in production.
    #[test]
    fn the_tail_is_an_axis_of_its_own() {
        let scores = [0.80, 0.81, 0.79, 0.80];
        let (f, _) = run(&[
            row("spiky", &scores, 0.01, 100, 4000),
            row("steady", &scores, 0.01, 200, 300),
        ]);
        assert_eq!(labels(&f, "non_dominated").len(), 2, "{f}");
    }
}
