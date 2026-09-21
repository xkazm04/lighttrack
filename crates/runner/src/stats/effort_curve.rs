//! **What one rung of the effort ladder bought over the rung below it.**
//!
//! The matrix could already put `model@low` and `model@high` side by side, and a reader could see
//! that the means differed by 0.03. Nothing said *why*, and the three explanations are not the same
//! decision: the model thought much harder and the corpus could not tell (the cases are too easy),
//! the model thought much harder and got some cases wrong that it had right (overthinking), or the
//! dial did nothing at all and the two rows are one call at two prices. The live 2026-09-07 run hit
//! the first and the third and could not distinguish them — the ~10× thinking rise on sonnet was
//! measured afterwards, by hand, outside the framework.
//!
//! Three measures, per adjacent pair of rungs, over the cases **both** rows judged:
//! - **Is the dial alive** — the ratio of median thinking tokens. Below [`DIAL_MOVED`] the rung
//!   bought no more thinking, so every rung above it is the same call at a higher price.
//! - **Which cases flipped** — wrong→right and right→wrong, counted separately. A mean delta of
//!   +0.00 hides three cases that improved and three that regressed, and the second number is the
//!   measurable form of overthinking.
//! - **What the extra thinking bought** — score per 1k extra thinking tokens, and the extra $ per
//!   case, per difficulty tier where the corpus is graded.
//!
//! **Descriptive, and it must stay so.** Like the per-tier scorecard (D25) this reports counts and
//! medians over one run and never dresses them as a test: no p, no α, no significance vocabulary.
//! There is a second reason here beyond per-step power. The steps are chosen *after* seeing which
//! targets the matrix happened to contain, and the run's own significance path is already corrected
//! across a family of target pairs (D23) — adding a second, differently-shaped family of comparisons
//! with its own α would make the corrected claims in the same report incomparable with each other.
//! A flip count is an observation about this corpus; if you want it tested, the tool for that is
//! more cases and the paired test that already exists.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use lighttrack_core::{Difficulty, Effort};

use super::thinking::{median, CaseSpend};
use super::tiers::{grade_of, ladder, Tier};
use super::EPS;
use crate::case_spend::ThinkingBasis;

/// The ratio of median thinking tokens below which a rung bought no more deliberation. Deliberately
/// generous: run-to-run noise on a median is small, and the case this exists to catch is a dial that
/// did *nothing* (ratio ≈ 1.0), not a rung that thought 15% longer.
const DIAL_MOVED: f64 = 1.2;

/// Fewer draws than this per case, and a flip cannot be told from sampling noise — most generation
/// paths are `best-effort` (Anthropic exposes no seed; the CLI exposes nothing), so one draw per rung
/// is one sample of a distribution.
const FLIPS_NEED_DRAWS: u32 = 3;

const DESCRIPTIVE: &str = "descriptive, within one model — what each rung of the effort ladder \
                           bought over the rung below it, on the cases both rungs judged. Counts \
                           and medians over this run, never a significance test";

/// One target that declared an effort, as the ladder sees it.
pub(crate) struct LadderRow<'a> {
    pub(crate) label: &'a str,
    /// What makes two rows the same experiment at two rungs: provider, bare model, and the prompt
    /// they generate with. Two efforts of one model under *different* prompts are two experiments,
    /// and stepping between them would attribute the prompt's effect to the effort.
    pub(crate) key: String,
    /// Display name for the group (`provider/model`).
    pub(crate) model: String,
    pub(crate) effort: Effort,
    pub(crate) cases: &'a [CaseSpend],
}

/// Layer `effort_curve` onto the **matrix summary**. Printed, not persisted, for the same reason the
/// frontier and the discrimination verdict are: it is inherently cross-target, and compare mode posts
/// one run per target from inside its loop so a crash mid-matrix still records what finished.
///
/// Absent entirely unless some model in the matrix was run at two or more efforts with the same
/// prompt — a matrix with no ladder renders exactly as it always did.
pub(crate) fn annotate_effort_curve(
    summary: &mut Value,
    rows: &[LadderRow],
    tiers: &[Option<Difficulty>],
    gen_samples: u32,
) {
    let mut groups: BTreeMap<&str, Vec<&LadderRow>> = BTreeMap::new();
    for r in rows {
        groups.entry(r.key.as_str()).or_default().push(r);
    }
    let mut models: Vec<Value> = Vec::new();
    for (_, mut rungs) in groups {
        rungs.sort_by_key(|r| r.effort);
        if rungs.len() < 2 {
            continue;
        }
        let steps: Vec<Value> = rungs
            .windows(2)
            .filter_map(|w| step(w[0], w[1], tiers))
            .collect();
        if !steps.is_empty() {
            models.push(json!({ "model": rungs[0].model, "steps": steps }));
        }
    }
    if models.is_empty() {
        return;
    }
    let mut caveats: Vec<String> = Vec::new();
    if gen_samples < FLIPS_NEED_DRAWS {
        caveats.push(format!(
            "one draw per case at each rung (--gen-samples {gen_samples}): a flipped case may be \
             sampling noise rather than the effort level, because most generation paths expose no \
             seed. Use --gen-samples {FLIPS_NEED_DRAWS} or more to separate the two"
        ));
    }
    if let Some(obj) = summary.as_object_mut() {
        obj.insert(
            "effort_curve".into(),
            json!({ "note": DESCRIPTIVE, "caveats": caveats, "models": models }),
        );
    }
}

/// The cases both rungs judged, in case order. Pairing is by case id for the reason D23 gives: these
/// vectors are compacted past each target's errored cells, so equal lengths never established that
/// they describe the same cases.
fn pair_by_case<'a>(
    from: &'a [CaseSpend],
    to: &'a [CaseSpend],
) -> Vec<(&'a CaseSpend, &'a CaseSpend)> {
    let index: BTreeMap<u32, &CaseSpend> = to.iter().map(|c| (c.case, c)).collect();
    let mut out: Vec<(&CaseSpend, &CaseSpend)> = from
        .iter()
        .filter_map(|f| index.get(&f.case).map(|t| (f, *t)))
        .collect();
    out.sort_by_key(|(f, _)| f.case);
    out
}

/// The strongest basis both rungs measured on **every** shared case. A basis that holds for some
/// cases and not others would put two different measures in one median.
fn shared_basis(pairs: &[(&CaseSpend, &CaseSpend)]) -> Option<ThinkingBasis> {
    [ThinkingBasis::Reasoning, ThinkingBasis::Output]
        .into_iter()
        .find(|&basis| {
            pairs
                .iter()
                .all(|(f, t)| f.thinking(basis).is_some() && t.thinking(basis).is_some())
        })
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len().max(1) as f64
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// How the flips fell on one set of paired cases.
#[derive(Debug, Default, Clone, Copy)]
struct Flips {
    improved: usize,
    regressed: usize,
    unchanged: usize,
}

fn flips(pairs: &[(&CaseSpend, &CaseSpend)]) -> Flips {
    let mut f = Flips::default();
    for (from, to) in pairs {
        match (from.pass, to.pass) {
            (false, true) => f.improved += 1,
            (true, false) => f.regressed += 1,
            _ => f.unchanged += 1,
        }
    }
    f
}

/// Medians of the thinking measure on each side, and the ratio between them.
fn thinking_medians(
    pairs: &[(&CaseSpend, &CaseSpend)],
    basis: ThinkingBasis,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let from: Vec<f64> = pairs
        .iter()
        .filter_map(|(f, _)| f.thinking(basis))
        .collect();
    let to: Vec<f64> = pairs
        .iter()
        .filter_map(|(_, t)| t.thinking(basis))
        .collect();
    let (a, b) = (median(&from), median(&to));
    let ratio = match (a, b) {
        (Some(a), Some(b)) if a > EPS => Some(b / a),
        _ => None,
    };
    (a, b, ratio)
}

/// The mean per-case difference of a figure both sides reported, or `None` when either did not.
fn mean_delta(
    pairs: &[(&CaseSpend, &CaseSpend)],
    pick: impl Fn(&CaseSpend) -> Option<f64>,
) -> Option<f64> {
    let deltas: Vec<f64> = pairs
        .iter()
        .map(|(f, t)| Some(pick(t)? - pick(f)?))
        .collect::<Option<Vec<f64>>>()?;
    (!deltas.is_empty()).then(|| mean(&deltas))
}

/// One rung-to-rung step, or `None` when the two rungs share no case.
fn step(from: &LadderRow, to: &LadderRow, tiers: &[Option<Difficulty>]) -> Option<Value> {
    let pairs = pair_by_case(from.cases, to.cases);
    if pairs.is_empty() {
        return None;
    }
    let basis = shared_basis(&pairs);
    let (med_from, med_to, ratio) = match basis {
        Some(b) => thinking_medians(&pairs, b),
        None => (None, None, None),
    };
    let f = flips(&pairs);
    let score_delta = mean(
        &pairs
            .iter()
            .map(|(a, b)| b.score - a.score)
            .collect::<Vec<f64>>(),
    );
    let cost_delta = mean_delta(&pairs, |c| c.cost_usd);
    let token_delta = basis.and_then(|b| mean_delta(&pairs, |c| c.thinking(b)));
    // Score per 1k extra thinking tokens: the yield the extra deliberation actually returned. Only
    // when the thinking rose — dividing by a fall would report a "yield" for spending less.
    let yield_per_1k = token_delta
        .filter(|d| *d > 1.0)
        .map(|d| score_delta / (d / 1000.0));
    let dial = match ratio {
        None => "unknown",
        Some(r) if r >= DIAL_MOVED => "alive",
        Some(_) => "flat",
    };
    let mut row = json!({
        "from": from.label, "to": to.label,
        "from_effort": from.effort.as_str(), "to_effort": to.effort.as_str(),
        "n_cases": pairs.len(),
        "basis": basis.map(|b| b.as_str()),
        "dial": dial,
        "median_thinking_from": med_from.map(round3),
        "median_thinking_to": med_to.map(round3),
        "thinking_ratio": ratio.map(|r| (r * 100.0).round() / 100.0),
        "improved": f.improved, "regressed": f.regressed, "unchanged": f.unchanged,
        "mean_score_delta": round3(score_delta),
        "cost_per_case_delta_usd": cost_delta,
        "score_per_1k_thinking": yield_per_1k.map(round3),
        "verdict": verdict(basis, dial, ratio, med_from, med_to, f, score_delta, cost_delta),
    });
    let tier_rows = tier_rows(&pairs, tiers, basis);
    if !tier_rows.is_empty() {
        row["tiers"] = json!(tier_rows);
    }
    Some(row)
}

/// The word for the count being compared — never "reasoning" when the number is output tokens
/// standing in for a split the provider did not report.
fn basis_word(basis: Option<ThinkingBasis>) -> &'static str {
    match basis {
        Some(ThinkingBasis::Reasoning) => "reasoning tokens",
        Some(ThinkingBasis::Output) => "output tokens",
        None => "tokens",
    }
}

#[allow(clippy::too_many_arguments)]
fn verdict(
    basis: Option<ThinkingBasis>,
    dial: &str,
    ratio: Option<f64>,
    med_from: Option<f64>,
    med_to: Option<f64>,
    f: Flips,
    score_delta: f64,
    cost_delta: Option<f64>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (ratio, med_from, med_to) {
        (Some(r), Some(a), Some(b)) => parts.push(format!(
            "thinking ×{r:.1} (median {a:.0}→{b:.0} {})",
            basis_word(basis)
        )),
        _ => parts.push("thinking not measured on both rungs".to_string()),
    }
    parts.push(format!(
        "{} improved, {} regressed, {} unchanged",
        f.improved, f.regressed, f.unchanged
    ));
    parts.push(format!("mean score {score_delta:+.3}"));
    if let Some(c) = cost_delta {
        parts.push(format!("{:+.5} $/case", c));
    }
    let tail = if dial == "unknown" {
        "whether the level changed how hard the model thought cannot be read from this run"
    } else if dial == "flat" {
        "the dial did not move — this rung is the rung below it at a higher price"
    } else if f.regressed > f.improved {
        "more cases flipped wrong than right: the extra thinking cost accuracy here"
    } else if f.improved == 0 && f.regressed == 0 {
        // No pass verdict moved. A score delta can still be non-zero here: with several draws per
        // case, one draw in three going wrong moves the case's mean without crossing the majority
        // that decides `pass`. The first live run hit exactly this and was told "the flips
        // cancelled out" about a step with no flips at all.
        if score_delta.abs() <= EPS {
            "the model thought harder and nothing about the verdicts changed"
        } else {
            "no case changed its pass verdict — the score moved only on individual draws inside \
             cases, which is sampling within a case rather than the level changing an answer"
        }
    } else if f.improved > f.regressed {
        "the extra thinking paid, on the cases that flipped"
    } else {
        "the flips cancelled out"
    };
    format!("{} — {tail}", parts.join("; "))
}

/// Per-tier rows for one step, in ladder order. Empty when the corpus carries no grades.
fn tier_rows(
    pairs: &[(&CaseSpend, &CaseSpend)],
    tiers: &[Option<Difficulty>],
    basis: Option<ThinkingBasis>,
) -> Vec<Value> {
    if !pairs.iter().any(|(f, _)| grade_of(tiers, f.case).is_some()) {
        return Vec::new();
    }
    ladder()
        .into_iter()
        .filter_map(|t| tier_row(t, pairs, tiers, basis))
        .collect()
}

fn tier_row(
    t: Tier,
    pairs: &[(&CaseSpend, &CaseSpend)],
    tiers: &[Option<Difficulty>],
    basis: Option<ThinkingBasis>,
) -> Option<Value> {
    let here: Vec<(&CaseSpend, &CaseSpend)> = pairs
        .iter()
        .filter(|(f, _)| t.holds(grade_of(tiers, f.case)))
        .copied()
        .collect();
    if here.is_empty() {
        return None;
    }
    let f = flips(&here);
    let delta = mean(
        &here
            .iter()
            .map(|(a, b)| b.score - a.score)
            .collect::<Vec<f64>>(),
    );
    let (med_from, med_to, ratio) = match basis {
        Some(b) => thinking_medians(&here, b),
        None => (None, None, None),
    };
    Some(json!({
        "tier": t.as_str(),
        "n_cases": here.len(),
        "mean_score_delta": round3(delta),
        "improved": f.improved,
        "regressed": f.regressed,
        "median_thinking_from": med_from.map(round3),
        "median_thinking_to": med_to.map(round3),
        "thinking_ratio": ratio.map(|r| (r * 100.0).round() / 100.0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A case with a score, a pass flag derived from it, and a thinking count on both bases.
    fn c(case: u32, score: f64, thinking: f64) -> CaseSpend {
        CaseSpend {
            case,
            score,
            pass: score >= 0.5,
            output_tokens: Some(thinking),
            reasoning_tokens: Some(thinking),
            cost_usd: Some(thinking / 100_000.0),
        }
    }

    /// A case whose provider reported no reasoning split — output tokens only.
    fn out_only(case: u32, score: f64, thinking: f64) -> CaseSpend {
        CaseSpend {
            reasoning_tokens: None,
            ..c(case, score, thinking)
        }
    }

    fn row<'a>(label: &'a str, effort: Effort, cases: &'a [CaseSpend]) -> LadderRow<'a> {
        LadderRow {
            label,
            key: "anthropic/sonnet|".to_string(),
            model: "anthropic/sonnet".to_string(),
            effort,
            cases,
        }
    }

    fn curve(rows: &[LadderRow], tiers: &[Option<Difficulty>], draws: u32) -> Value {
        let mut summary = json!({ "n_cases": 3 });
        annotate_effort_curve(&mut summary, rows, tiers, draws);
        summary
    }

    fn graded(spec: &[&str]) -> Vec<Option<Difficulty>> {
        spec.iter().map(|s| Difficulty::parse(s)).collect()
    }

    /// **THE LIVE FINDING, as a measurement.** Sonnet's thinking rose ~10× from low to high and its
    /// correctness did not move. The matrix could show the two means; only this says the dial was
    /// alive and bought nothing, which is a different decision from a dial that never moved.
    #[test]
    fn a_live_dial_that_buys_nothing_is_named_as_such() {
        let low = [c(1, 1.0, 360.0), c(2, 0.0, 340.0), c(3, 1.0, 380.0)];
        let high = [c(1, 1.0, 3472.0), c(2, 0.0, 3600.0), c(3, 1.0, 3300.0)];
        let rows = [
            row("sonnet@low", Effort::Low, &low),
            row("sonnet@high", Effort::High, &high),
        ];
        let v = curve(&rows, &[None; 3], 3);
        let step = &v["effort_curve"]["models"][0]["steps"][0];
        assert_eq!(step["dial"], json!("alive"));
        assert_eq!(step["basis"], json!("reasoning_tokens"));
        assert!((step["thinking_ratio"].as_f64().unwrap() - 9.64).abs() < 0.05);
        assert_eq!(step["improved"], json!(0));
        assert_eq!(step["regressed"], json!(0));
        assert_eq!(step["mean_score_delta"], json!(0.0));
        let verdict = step["verdict"].as_str().unwrap();
        assert!(verdict.contains("thinking ×9.6"), "{verdict}");
        assert!(
            verdict.contains("nothing about the verdicts changed"),
            "{verdict}"
        );
        // Descriptive: no significance vocabulary anywhere in the block.
        let block = v["effort_curve"].to_string();
        for banned in ["p_value", "significant", "alpha", "α"] {
            assert!(!block.contains(banned), "{banned} leaked: {block}");
        }
    }

    /// **Found by the first live run.** With 3 draws per case, one draw going wrong moves a case's
    /// mean (1.00 → 0.67) without moving its pass verdict. That step has zero flips and a non-zero
    /// score delta, and the report called it "the flips cancelled out" — a sentence about flips that
    /// did not happen.
    #[test]
    fn a_score_that_moved_without_a_flip_is_not_called_cancelled_flips() {
        let low = [c(21, 0.667, 250.0), c(22, 1.0, 250.0)];
        let high = [c(21, 1.0, 400.0), c(22, 1.0, 400.0)];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &[None; 22], 3)["effort_curve"]["models"][0]["steps"][0].clone();
        assert_eq!(step["improved"], json!(0));
        assert_eq!(step["regressed"], json!(0));
        assert!(step["mean_score_delta"].as_f64().unwrap() > 0.0);
        let verdict = step["verdict"].as_str().unwrap();
        assert!(!verdict.contains("cancelled out"), "{verdict}");
        assert!(
            verdict.contains("no case changed its pass verdict"),
            "{verdict}"
        );
    }

    /// Genuine cancellation still says so: equal, non-zero flips in each direction.
    #[test]
    fn equal_flips_in_both_directions_still_cancel_out() {
        let low = [c(1, 0.0, 100.0), c(2, 1.0, 100.0)];
        let high = [c(1, 1.0, 900.0), c(2, 0.0, 900.0)];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &[None; 2], 3)["effort_curve"]["models"][0]["steps"][0].clone();
        assert_eq!(
            (step["improved"].clone(), step["regressed"].clone()),
            (json!(1), json!(1))
        );
        assert!(step["verdict"]
            .as_str()
            .unwrap()
            .contains("the flips cancelled out"));
    }

    /// **The dead dial.** Asking for more effort changed nothing about how hard the model thought —
    /// so every rung above this one is the same call at a higher price, and the run says so instead
    /// of leaving an operator to read two near-identical means as a quality finding.
    #[test]
    fn a_dial_that_did_not_move_is_reported_as_the_same_call_at_a_higher_price() {
        let low = [c(1, 1.0, 300.0), c(2, 1.0, 310.0)];
        let high = [c(1, 1.0, 305.0), c(2, 1.0, 315.0)];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &[None; 2], 3)["effort_curve"]["models"][0]["steps"][0].clone();
        assert_eq!(step["dial"], json!("flat"));
        assert!(
            step["verdict"]
                .as_str()
                .unwrap()
                .contains("the dial did not move"),
            "{step}"
        );
    }

    /// **Overthinking, counted.** A mean delta of ~0 hides two cases that improved and three that
    /// regressed; the flip counts are what make it visible, and the sentence names it.
    #[test]
    fn right_to_wrong_flips_are_counted_apart_from_wrong_to_right() {
        let low = [
            c(1, 0.0, 100.0),
            c(2, 0.0, 100.0),
            c(3, 1.0, 100.0),
            c(4, 1.0, 100.0),
            c(5, 1.0, 100.0),
        ];
        let high = [
            c(1, 1.0, 900.0),
            c(2, 1.0, 900.0),
            c(3, 0.0, 900.0),
            c(4, 0.0, 900.0),
            c(5, 0.0, 900.0),
        ];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &[None; 5], 3)["effort_curve"]["models"][0]["steps"][0].clone();
        assert_eq!(step["improved"], json!(2));
        assert_eq!(step["regressed"], json!(3));
        assert_eq!(step["mean_score_delta"], json!(-0.2));
        assert!(
            step["verdict"]
                .as_str()
                .unwrap()
                .contains("more cases flipped wrong than right"),
            "{step}"
        );
        // The yield is negative, and per 1k of the tokens that were actually spent.
        assert!(step["score_per_1k_thinking"].as_f64().unwrap() < 0.0);
    }

    /// Yield per tier: effort pays on the hard rung and buys nothing on the easy one — the finding
    /// a corpus-wide mean cannot express.
    #[test]
    fn the_step_is_broken_down_by_tier_when_the_corpus_is_graded() {
        let tiers = graded(&["easy", "easy", "hard", "hard"]);
        let low = [
            c(1, 1.0, 100.0),
            c(2, 1.0, 100.0),
            c(3, 0.0, 200.0),
            c(4, 0.0, 200.0),
        ];
        let high = [
            c(1, 1.0, 800.0),
            c(2, 1.0, 800.0),
            c(3, 1.0, 4000.0),
            c(4, 1.0, 4000.0),
        ];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &tiers, 3)["effort_curve"]["models"][0]["steps"][0].clone();
        let tier = |name: &str| {
            step["tiers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["tier"] == json!(name))
                .cloned()
                .unwrap()
        };
        assert_eq!(tier("easy")["mean_score_delta"], json!(0.0));
        assert_eq!(tier("easy")["improved"], json!(0));
        assert_eq!(tier("hard")["mean_score_delta"], json!(1.0));
        assert_eq!(tier("hard")["improved"], json!(2));
        assert_eq!(tier("hard")["median_thinking_to"], json!(4000.0));
    }

    /// Output tokens stand in where a provider reports no reasoning split — and are never labelled
    /// reasoning. This is the `claude -p` and Anthropic-API case, i.e. most of this repo's runs.
    #[test]
    fn output_tokens_are_the_basis_when_no_split_is_reported_and_say_so() {
        let low = [out_only(1, 1.0, 200.0), out_only(2, 1.0, 200.0)];
        let high = [out_only(1, 1.0, 2000.0), out_only(2, 1.0, 2000.0)];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &[None; 2], 3)["effort_curve"]["models"][0]["steps"][0].clone();
        assert_eq!(step["basis"], json!("output_tokens"));
        let verdict = step["verdict"].as_str().unwrap();
        assert!(verdict.contains("output tokens"), "{verdict}");
        assert!(
            !verdict.contains("reasoning"),
            "never mislabelled: {verdict}"
        );
    }

    /// Two rungs that judged different cases are compared on their intersection, by case id — the
    /// same rule the paired test follows (D23), because these vectors are compacted the same way.
    #[test]
    fn rungs_are_paired_on_the_cases_both_judged() {
        let low = [c(1, 0.0, 100.0), c(2, 1.0, 100.0), c(3, 1.0, 100.0)];
        let high = [c(2, 1.0, 900.0), c(3, 0.0, 900.0), c(9, 1.0, 900.0)];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let step = curve(&rows, &[None; 3], 3)["effort_curve"]["models"][0]["steps"][0].clone();
        assert_eq!(step["n_cases"], json!(2), "cases 2 and 3 only");
        assert_eq!(step["improved"], json!(0));
        assert_eq!(step["regressed"], json!(1));
    }

    /// A single-draw run says a flip may be sampling noise. Most generation paths expose no seed, so
    /// this caveat is the honest default rather than an edge case.
    #[test]
    fn a_single_draw_run_carries_the_sampling_caveat() {
        let low = [c(1, 0.0, 100.0)];
        let high = [c(1, 1.0, 900.0)];
        let rows = [
            row("m@low", Effort::Low, &low),
            row("m@high", Effort::High, &high),
        ];
        let one = curve(&rows, &[None; 1], 1);
        let caveats = one["effort_curve"]["caveats"].as_array().unwrap();
        assert_eq!(caveats.len(), 1);
        assert!(caveats[0].as_str().unwrap().contains("sampling noise"));
        // With enough draws the caveat is gone rather than reworded.
        let three = curve(&rows, &[None; 1], 3);
        assert!(three["effort_curve"]["caveats"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// No ladder, no block: one effort, or two efforts of *different* experiments (different prompts,
    /// different models), leave the summary exactly as it was.
    #[test]
    fn a_matrix_with_no_ladder_gets_no_block() {
        let cases = [c(1, 1.0, 100.0)];
        let single = [row("m@low", Effort::Low, &cases)];
        assert!(curve(&single, &[None; 1], 3).get("effort_curve").is_none());

        // Same model, two efforts, but different prompts — not one ladder.
        let a = LadderRow {
            key: "anthropic/sonnet|terse".into(),
            ..row("m@low", Effort::Low, &cases)
        };
        let b = LadderRow {
            key: "anthropic/sonnet|verbose".into(),
            ..row("m@high", Effort::High, &cases)
        };
        assert!(curve(&[a, b], &[None; 1], 3).get("effort_curve").is_none());
    }
}
