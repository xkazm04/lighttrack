//! The per-tier scorecard, and the one thing a difficulty ladder exists to answer: **which part of
//! the corpus did any work**.
//!
//! A live 6-target matrix (`{haiku,sonnet,opus}` × `{low,high}`, 2026-09-07) over graded cases scored
//! 1.00 on every `easy` and every `medium` case, from all six targets. 36 of that round's 54
//! generation calls bought no information at all — two thirds of its wall-clock and its spend — and
//! nothing in the framework could say so. `BenchmarkCase::difficulty` (M27) was carried all the way
//! into the runner and dropped on the floor, so the scorecard printed six columns of aggregate means
//! and never mentioned where the separation came from. The finding came from a hand-written script
//! hitting the API afterwards.
//!
//! Two outputs, deliberately in two places:
//! - **Per target**, the mean and case count per rung, layered onto that target's run report — which
//!   is POSTed, and therefore readable later by `get_benchmark_runs`, a CI gate and MCP.
//! - **Across targets**, the discrimination verdict, which is inherently cross-target and so lives
//!   only on the printed matrix summary. Compare mode posts one run per target from *inside* its
//!   per-target loop so a crash mid-matrix still records the targets that finished; deferring those
//!   posts to gain a persisted matrix artefact would trade a real durability property for a
//!   reporting nicety. Same trade, same answer, as the frontier — see BENCHMARK_FRAMEWORK §2b.
//!
//! **The verdict is DESCRIPTIVE.** "Every target scored the same on this tier" is an observation
//! about this run, not a statistical claim, and it is never dressed as one: no p, no α, no
//! significance vocabulary, and explicitly no per-tier recommendation. Per-tier power is far worse
//! than the run's — the tier that *did* discriminate in that live run had three cases — and a
//! per-tier "cheapest sufficient" would be precisely the confident-on-nothing failure the existing
//! power disclosure exists to prevent.

use serde_json::{json, Value};

use lighttrack_core::Difficulty;

use super::cases::CaseScore;
use super::{round3, EPS};

/// One bucket of the table: a rung of the ladder, or the ungraded remainder.
///
/// `Ungraded` is a bucket and never a rung. `None` means *ungraded*, not *medium*, everywhere in this
/// codebase, and a per-tier table that quietly dropped these cases would imply a coverage it does not
/// have — the live run had 8 of its 18 cases ungraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tier {
    Graded(Difficulty),
    Ungraded,
}

impl Tier {
    /// Whether a case with grade `g` belongs in this bucket. Shared with the thinking measures, so
    /// "which rung is this case on" is answered once for every per-tier table in the runner.
    pub(super) fn holds(self, g: Option<Difficulty>) -> bool {
        in_tier(self, g)
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Tier::Graded(d) => d.as_str(),
            Tier::Ungraded => "ungraded",
        }
    }

    /// What the verdict sentence calls this bucket. A rung and the remainder are different things and
    /// a sentence that called both "this tier" would have promoted the absence of a grade to a grade.
    fn noun(self) -> &'static str {
        match self {
            Tier::Graded(_) => "this tier",
            Tier::Ungraded => "the ungraded bucket",
        }
    }
}

/// Every bucket in ascending ladder order, ungraded last. The ONLY order any of this is emitted in:
/// `Difficulty` is an ordered three-rung ladder and ordering is the whole reason it is an enum rather
/// than a tag, so a table in hash order would throw away the one property the type carries.
pub(super) fn ladder() -> Vec<Tier> {
    Difficulty::ALL
        .iter()
        .copied()
        .map(Tier::Graded)
        .chain(std::iter::once(Tier::Ungraded))
        .collect()
}

/// The grade of the case a score was measured on. Case ids are 1-based (`i + 1`, the same identity
/// the run report writes on every logged case); an id outside the corpus reads as ungraded rather
/// than panicking, because a report is not a place to discover an off-by-one.
pub(super) fn grade_of(tiers: &[Option<Difficulty>], case: u32) -> Option<Difficulty> {
    case.checked_sub(1)
        .and_then(|i| tiers.get(i as usize))
        .copied()
        .flatten()
}

fn in_tier(t: Tier, g: Option<Difficulty>) -> bool {
    match t {
        Tier::Graded(d) => g == Some(d),
        Tier::Ungraded => g.is_none(),
    }
}

/// This target's scores inside one bucket.
fn scores_in(scores: &[CaseScore], tiers: &[Option<Difficulty>], t: Tier) -> Vec<f64> {
    scores
        .iter()
        .filter(|c| in_tier(t, grade_of(tiers, c.case)))
        .map(|c| c.score)
        .collect()
}

fn mean_of(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len().max(1) as f64
}

/// Cases the **corpus** carries at one tier — the denominator the operator paid for, which a target
/// that errored on one of them does not reach.
fn corpus_cases(tiers: &[Option<Difficulty>], t: Tier) -> usize {
    tiers.iter().filter(|g| in_tier(t, **g)).count()
}

/// Does any scored case carry a grade at all? A corpus that is entirely ungraded gets **no table** —
/// not an empty one and not a zero-filled one. There is nothing to say, and a table of one `ungraded`
/// row would be a per-tier view of a corpus that has no tiers.
fn any_graded(scores: &[CaseScore], tiers: &[Option<Difficulty>]) -> bool {
    scores.iter().any(|c| grade_of(tiers, c.case).is_some())
}

/// Layer `tiers` — one row per non-empty bucket, in ladder order — onto **one target's** run report.
/// Additive JSON: a run over an ungraded corpus lacks the key entirely, exactly as every run made
/// before this existed does.
///
/// The count travels with the mean always. A tier mean over 2 cases and one over 40 are not the same
/// evidence, and a reader must not have to guess which of the two they are looking at.
pub(crate) fn annotate_tiers(
    report: &mut Value,
    scores: &[CaseScore],
    tiers: &[Option<Difficulty>],
) {
    if !any_graded(scores, tiers) {
        return;
    }
    let rows: Vec<Value> = ladder()
        .into_iter()
        .filter_map(|t| {
            let xs = scores_in(scores, tiers, t);
            (!xs.is_empty()).then(
                || json!({ "tier": t.as_str(), "mean": round3(mean_of(&xs)), "n_cases": xs.len() }),
            )
        })
        .collect();
    if let Some(obj) = report.as_object_mut() {
        obj.insert("tiers".into(), json!(rows));
    }
}

/// The header that travels with the cross-target block, so the claim can never be read as a test.
const DESCRIPTIVE: &str = "descriptive, across targets — whether this run's targets actually scored \
                           differently on each tier. Not a significance test: per-tier power is far \
                           below the run's, so no per-tier recommendation is made";

/// Layer `tier_discrimination` onto the **matrix summary**: for each bucket, the spread of the
/// targets' per-tier means and a plain sentence saying whether it separated anything.
///
/// A tier every target scored identically on carries zero information and the operator is paying for
/// it — and so, symmetrically, does a tier every target fails: the spread is 0 at the bottom instead
/// of the top, and the same sentence is the honest one.
pub(crate) fn annotate_discrimination(
    summary: &mut Value,
    targets: &[(&str, &[CaseScore])],
    tiers: &[Option<Difficulty>],
) {
    if !targets.iter().any(|(_, s)| any_graded(s, tiers)) {
        return;
    }
    let rows: Vec<Value> = ladder()
        .into_iter()
        .filter_map(|t| tier_row(t, targets, tiers))
        .collect();
    if rows.is_empty() {
        return;
    }
    if let Some(obj) = summary.as_object_mut() {
        obj.insert(
            "tier_discrimination".into(),
            json!({ "note": DESCRIPTIVE, "tiers": rows }),
        );
    }
}

/// One bucket's cross-target row, or `None` when no target scored a case in it.
fn tier_row(
    t: Tier,
    targets: &[(&str, &[CaseScore])],
    tiers: &[Option<Difficulty>],
) -> Option<Value> {
    let mut per: Vec<(&str, f64, usize)> = targets
        .iter()
        .filter_map(|(label, s)| {
            let xs = scores_in(s, tiers, t);
            (!xs.is_empty()).then(|| (*label, mean_of(&xs), xs.len()))
        })
        .collect();
    if per.is_empty() {
        return None;
    }
    // Sorted by mean with the label as the tie-break, so two runs of one matrix name the same two
    // targets at the ends of the range instead of whichever the target order happened to put there.
    per.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(b.0)));
    let (low, high) = (per[0], per[per.len() - 1]);
    let spread = high.1 - low.1;
    let corpus = corpus_cases(tiers, t);
    // A target that errored inside this tier has a mean over fewer cases than the tier holds. Said,
    // never left for the reader to infer from a count that looks complete.
    let uneven = per.iter().any(|(_, _, n)| *n < corpus);
    let noun = t.noun();
    let mut verdict = if per.len() < 2 {
        format!("only one target was scored here — {noun} has nothing to separate")
    } else if spread <= EPS {
        format!(
            "every target scored {:.2} (spread {:.2}) — {noun} separated no targets",
            low.1, spread
        )
    } else {
        format!(
            "targets ranged {:.2} ({}) to {:.2} ({}), spread {:.2} — {noun} separated targets",
            low.1, low.0, high.1, high.0, spread
        )
    };
    if uneven {
        verdict.push_str(&format!(
            "; some targets were scored on fewer than the {corpus} case(s) graded here, so their \
             means cover less of it"
        ));
    }
    Some(json!({
        "tier": t.as_str(),
        "cases": corpus,
        "targets": per.len(),
        "low": round3(low.1), "low_label": low.0,
        "high": round3(high.1), "high_label": high.0,
        "spread": round3(spread),
        // `null`, not `false`, with a single target: "did it separate them?" has no answer rather
        // than the answer "no" — the same shape `all_candidates_indistinguishable` takes.
        "separates": if per.len() < 2 { Value::Null } else { json!(spread > EPS) },
        "uneven_coverage": uneven,
        "verdict": verdict,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus's grades, one per case in case order.
    fn graded(spec: &[&str]) -> Vec<Option<Difficulty>> {
        spec.iter().map(|s| Difficulty::parse(s)).collect()
    }

    /// Per-case scores over cases 1..n — a target that judged every case.
    fn seq(scores: &[f64]) -> Vec<CaseScore> {
        scores
            .iter()
            .enumerate()
            .map(|(i, s)| CaseScore::new(i as u32 + 1, *s))
            .collect()
    }

    /// Per-case scores over an explicit case set — a target that errored on the rest.
    fn at(cases: &[u32], scores: &[f64]) -> Vec<CaseScore> {
        cases
            .iter()
            .zip(scores)
            .map(|(c, s)| CaseScore::new(*c, *s))
            .collect()
    }

    fn rows_of(report: &Value) -> Vec<(String, f64, u64)> {
        report["tiers"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|r| {
                (
                    r["tier"].as_str().unwrap_or_default().to_string(),
                    r["mean"].as_f64().unwrap_or_default(),
                    r["n_cases"].as_u64().unwrap_or_default(),
                )
            })
            .collect()
    }

    fn discriminate(targets: &[(&str, Vec<CaseScore>)], tiers: &[Option<Difficulty>]) -> Value {
        let borrowed: Vec<(&str, &[CaseScore])> =
            targets.iter().map(|(l, s)| (*l, s.as_slice())).collect();
        let mut summary = json!({ "n_cases": tiers.len() });
        annotate_discrimination(&mut summary, &borrowed, tiers);
        summary
    }

    fn tier(v: &Value, name: &str) -> Value {
        v["tier_discrimination"]["tiers"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["tier"] == json!(name)))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// **THE LIVE SCENARIO, round 1.** Six targets, an `easy` tier every one of them scores 1.00 on
    /// and a `hard` tier they differ on. The run that motivated this printed six columns of means and
    /// could not say that two thirds of its calls bought nothing.
    #[test]
    fn the_round_one_matrix_names_easy_as_carrying_no_information() {
        let tiers = graded(&["easy", "easy", "easy", "hard", "hard", "hard"]);
        let hard = [
            [1.0, 1.0, 0.5],
            [1.0, 0.5, 0.5],
            [1.0, 1.0, 1.0],
            [0.5, 0.5, 0.5],
            [1.0, 0.5, 1.0],
            [0.5, 1.0, 0.5],
        ];
        let labels = [
            "haiku@low",
            "haiku@high",
            "sonnet@low",
            "sonnet@high",
            "opus@low",
            "opus@high",
        ];
        let targets: Vec<(&str, Vec<CaseScore>)> = labels
            .iter()
            .zip(hard)
            .map(|(l, h)| {
                let mut s = vec![1.0, 1.0, 1.0];
                s.extend(h);
                (*l, seq(&s))
            })
            .collect();
        let v = discriminate(&targets, &tiers);

        let easy = tier(&v, "easy");
        assert_eq!(easy["separates"], json!(false), "{easy}");
        assert_eq!(easy["spread"], json!(0.0));
        assert_eq!(easy["cases"], json!(3), "the count is beside the verdict");
        assert_eq!(easy["targets"], json!(6));
        assert!(
            easy["verdict"].as_str().unwrap_or_default().contains(
                "every target scored 1.00 (spread 0.00) — this tier separated no targets"
            ),
            "the sentence that would have saved two thirds of the run: {easy}"
        );

        let hard_row = tier(&v, "hard");
        assert_eq!(hard_row["separates"], json!(true), "{hard_row}");
        assert!(
            hard_row["verdict"]
                .as_str()
                .unwrap_or_default()
                .contains("separated targets"),
            "{hard_row}"
        );
        assert!(hard_row["spread"].as_f64().unwrap_or_default() > 0.0);
        // Not a significance claim anywhere in the block — this is an observation about the run.
        let block = v["tier_discrimination"].to_string();
        for banned in ["p_value", "alpha", "significant", "α"] {
            assert!(
                !block.contains(banned),
                "{banned} leaked into a descriptive verdict: {block}"
            );
        }
    }

    /// A tier every target *fails* carries exactly as little: spread 0 at the bottom instead of the
    /// top, and the same sentence is the honest one.
    #[test]
    fn a_tier_every_target_fails_separates_nothing_either() {
        let tiers = graded(&["hard", "hard"]);
        let v = discriminate(
            &[
                ("a", seq(&[0.0, 0.0])),
                ("b", seq(&[0.0, 0.0])),
                ("c", seq(&[0.0, 0.0])),
            ],
            &tiers,
        );
        let hard = tier(&v, "hard");
        assert_eq!(hard["separates"], json!(false), "{hard}");
        assert!(
            hard["verdict"]
                .as_str()
                .unwrap_or_default()
                .contains("every target scored 0.00"),
            "a floor is as uninformative as a ceiling: {hard}"
        );
    }

    /// Every rung present, in ascending ladder order with ungraded LAST — never hash order.
    #[test]
    fn tiers_render_in_ascending_ladder_order_with_ungraded_last() {
        let tiers = graded(&["hard", "easy", "nonsense", "medium", "easy"]);
        let mut report = json!({ "mode": "compare" });
        annotate_tiers(&mut report, &seq(&[0.2, 1.0, 0.6, 0.5, 0.8]), &tiers);
        let names: Vec<String> = rows_of(&report).into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(
            names,
            vec!["easy", "medium", "hard", "ungraded"],
            "{report}"
        );
    }

    /// Ungraded cases are their own bucket and are NEVER counted into a rung — the failure that would
    /// make a tier mean look like it covered the corpus when 8 of 18 cases were ungraded.
    #[test]
    fn ungraded_cases_are_never_folded_into_a_rung() {
        let tiers = graded(&["easy", "easy", "", "", ""]);
        let mut report = json!({});
        annotate_tiers(&mut report, &seq(&[1.0, 1.0, 0.0, 0.0, 0.0]), &tiers);
        let rows = rows_of(&report);
        assert_eq!(
            rows,
            vec![
                ("easy".to_string(), 1.0, 2),
                ("ungraded".to_string(), 0.0, 3),
            ],
            "the rung mean is over its own 2 cases, not diluted by the 3 ungraded ones: {report}"
        );
        // The buckets sum to the judged count, so a reader can see the tiers do not cover it alone.
        assert_eq!(rows.iter().map(|r| r.2).sum::<u64>(), 5);
    }

    /// A corpus with no grades at all produces **no table** — not an empty one, not a zero-filled one.
    #[test]
    fn an_entirely_ungraded_corpus_produces_no_tier_table_at_all() {
        let tiers = vec![None, None, None];
        let mut report = json!({ "mode": "compare" });
        annotate_tiers(&mut report, &seq(&[0.5, 0.5, 0.5]), &tiers);
        assert!(report.get("tiers").is_none(), "{report}");
        let v = discriminate(
            &[("a", seq(&[0.5, 0.5, 0.5])), ("b", seq(&[0.9, 0.9, 0.9]))],
            &tiers,
        );
        assert!(v.get("tier_discrimination").is_none(), "{v}");
    }

    /// Only one rung graded: one row, and no empty rows for the rungs the corpus never used. Pins
    /// the **exact** persisted shape — this object is what a CI gate and MCP read back, so a renamed
    /// key is a silent break in a consumer this crate cannot see.
    #[test]
    fn a_corpus_with_one_tier_gets_one_row() {
        let tiers = graded(&["hard", "hard"]);
        let mut report = json!({ "mode": "compare" });
        annotate_tiers(&mut report, &seq(&[0.4, 0.6]), &tiers);
        assert_eq!(
            report["tiers"],
            json!([{ "tier": "hard", "mean": 0.5, "n_cases": 2 }]),
            "{report}"
        );
    }

    /// A tier holding a single case says so — a mean over one case must not read like a mean over
    /// forty, and the count is the only thing that distinguishes them.
    #[test]
    fn a_single_case_tier_reports_its_count() {
        let tiers = graded(&["easy", "hard"]);
        let mut report = json!({});
        annotate_tiers(&mut report, &seq(&[1.0, 0.25]), &tiers);
        assert_eq!(
            rows_of(&report),
            vec![("easy".to_string(), 1.0, 1), ("hard".to_string(), 0.25, 1),]
        );
    }

    /// One target errored inside a tier, so its mean covers less of it than the other's. Disclosed on
    /// the row rather than left inside a count that looks complete.
    #[test]
    fn an_errored_case_inside_a_tier_is_disclosed_as_uneven_coverage() {
        let tiers = graded(&["hard", "hard", "hard"]);
        let v = discriminate(
            &[
                ("full", seq(&[0.9, 0.9, 0.9])),
                ("errored", at(&[1, 3], &[0.2, 0.2])),
            ],
            &tiers,
        );
        let hard = tier(&v, "hard");
        assert_eq!(hard["uneven_coverage"], json!(true), "{hard}");
        assert_eq!(
            hard["cases"],
            json!(3),
            "the corpus count, not the smaller one"
        );
        assert!(
            hard["verdict"]
                .as_str()
                .unwrap_or_default()
                .contains("fewer than the 3 case(s) graded here"),
            "{hard}"
        );
    }

    /// One target alone on a tier separates nothing, and that is not the same fact as several targets
    /// scoring alike — `null`, never `false`.
    #[test]
    fn a_tier_only_one_target_reached_has_no_separation_answer() {
        let tiers = graded(&["easy", "hard"]);
        let v = discriminate(&[("a", seq(&[1.0, 0.5])), ("b", at(&[1], &[1.0]))], &tiers);
        assert_eq!(tier(&v, "hard")["separates"], Value::Null, "{v}");
        assert!(tier(&v, "hard")["verdict"]
            .as_str()
            .unwrap_or_default()
            .contains("only one target was scored here"));
        // …while the tier both reached still gets a real answer.
        assert_eq!(tier(&v, "easy")["separates"], json!(false));
    }

    /// Two runs of one matrix must produce the same table: neither the target order nor the order a
    /// target's own case vector was assembled in may change a row, a name, or the ordering.
    #[test]
    fn two_runs_of_one_matrix_produce_an_identical_table() {
        let tiers = graded(&["easy", "hard", "medium", "hard"]);
        let a = seq(&[1.0, 0.4, 0.8, 0.6]);
        let b = seq(&[1.0, 0.9, 0.7, 0.5]);
        let first = discriminate(&[("a", a.clone()), ("b", b.clone())], &tiers);
        // Same run, targets visited in the other order and one vector shuffled.
        let mut shuffled = a.clone();
        shuffled.reverse();
        let second = discriminate(&[("b", b), ("a", shuffled)], &tiers);
        assert_eq!(
            first, second,
            "the table is a function of the run, not of iteration order"
        );
        let order: Vec<&str> = first["tier_discrimination"]["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tier"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(order, vec!["easy", "medium", "hard"]);
    }

    /// A case id past the end of the corpus is ungraded, not a panic: a report is not a place to
    /// discover an off-by-one.
    #[test]
    fn a_case_id_outside_the_corpus_reads_as_ungraded() {
        let tiers = graded(&["easy"]);
        let mut report = json!({});
        annotate_tiers(&mut report, &at(&[1, 99], &[1.0, 0.0]), &tiers);
        assert_eq!(
            rows_of(&report),
            vec![
                ("easy".to_string(), 1.0, 1),
                ("ungraded".to_string(), 0.0, 1),
            ]
        );
    }
}
