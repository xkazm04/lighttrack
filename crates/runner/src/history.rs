//! Finding the **previous comparable run** of a benchmark, so this run's verdict can be paired
//! per case instead of compared mean-to-scalar.
//!
//! "Comparable" is deliberately strict. A paired test over case sets that aren't the same cases is
//! worse than no paired test at all — it would silently attribute a dataset change to the model —
//! so a candidate run must match on mode, target and (when both recorded it, which is every run
//! since candidate generation was pinned) dataset version.
//!
//! What it must *not* do is decide "the same cases" by counting them. That is what it used to do
//! (`s.len() == n_cases`), and a compare run's per-case vector is compacted past errored cells, so a
//! baseline that errored on case 3 and a run that errored on case 9 matched on count and paired one
//! case out of step from there on. The case identity was in the report all along —
//! `case_reports.push(json!({ "case": i + 1, … }))` — and read back discarded. It is now read, and
//! the alignment is done on it by [`crate::stats::paired_deltas_by_case`].

use lighttrack_core::BenchmarkRun;
use serde_json::Value;

use crate::stats::CaseScore;

/// A previous run's per-case scores, with what the reader needs to know about how complete they are.
pub(crate) struct PreviousRun {
    pub(crate) scores: Vec<CaseScore>,
    /// The baseline's `cases` array was clipped by [`crate::bench::attach_cases`], which keeps the
    /// FIRST `MAX_LOGGED_CASES`. Under the old count-match this made a large run yield *no* baseline
    /// at all; under case alignment it yields one over that prefix — the same cases in both runs, so
    /// a valid paired test, but a systematic subset rather than a random one. That is more useful
    /// than nothing **provided it is said out loud**, which is what this flag is for.
    pub(crate) preview_limited: bool,
}

/// The per-case scores a compare-mode run report recorded, each carrying the case it measured.
/// `None` when the report predates per-case reporting, or when any case is missing its numeric
/// score, or when any case is missing the `case` index itself.
///
/// The last of those is the strict half. A report with no `case` field cannot be aligned by
/// identity, and falling back to the array position would be exactly the defect this module was
/// rewritten to remove — the position is a rank among *judged* cases, not a case number. Compare
/// mode has written `case` on every logged case since 2026-05-31 (`962e04a`), so this only excludes
/// reports older than that, and the verdict says a baseline was not found rather than pairing one
/// it cannot trust.
pub(crate) fn case_scores(report: &Value) -> Option<Vec<CaseScore>> {
    let cases = report.get("cases")?.as_array()?;
    if cases.is_empty() {
        return None;
    }
    cases
        .iter()
        .map(|c| {
            let case = c.get("case").and_then(Value::as_u64)?;
            let score = c.get("score").and_then(Value::as_f64)?;
            Some(CaseScore::new(u32::try_from(case).ok()?, score))
        })
        .collect()
}

/// The dataset version a run was scored over, when it recorded one.
fn dataset_version(report: &Value) -> Option<u64> {
    report.get("dataset_version").and_then(Value::as_u64)
}

/// Whether a report's `cases` array is a bounded preview of a longer list.
fn preview_limited(report: &Value) -> bool {
    report.get("cases_truncated").and_then(Value::as_bool) == Some(true)
}

/// Per-case scores from the most recent finished run that scored `target` in compare mode over a
/// comparable case set. `current` is this run's own per-case scores; `dsv` its dataset version (when
/// known).
///
/// Runs are considered newest-`finished_at`-first, and the newest one that shares **at least one
/// case** with `current` wins. Sharing a case rather than sharing the count is the whole change: the
/// count never established that the two runs measured the same things, and the intersection is what
/// the paired test will run over anyway.
///
/// A run still in flight (`finished_at = None`) is skipped: an unfinished run's report is not a
/// baseline.
pub(crate) fn previous_case_scores(
    runs: &[BenchmarkRun],
    target: &str,
    current: &[CaseScore],
    dsv: Option<u64>,
) -> Option<PreviousRun> {
    let mine: std::collections::BTreeSet<u32> = current.iter().map(|c| c.case).collect();
    let mut candidates: Vec<&BenchmarkRun> = runs
        .iter()
        .filter(|r| {
            r.finished_at.is_some()
                && r.report.get("mode").and_then(Value::as_str) == Some("compare")
                && r.report.get("target").and_then(Value::as_str) == Some(target)
                // Only reject on a dataset-version *mismatch*: a legacy run that recorded none is
                // still usable, and saying so is more honest than silently having no baseline.
                && match (dsv, dataset_version(&r.report)) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                }
        })
        .collect();
    candidates.sort_by_key(|r| r.finished_at);
    candidates.iter().rev().find_map(|r| {
        let scores = case_scores(&r.report)?;
        scores
            .iter()
            .any(|c| mine.contains(&c.case))
            .then(|| PreviousRun {
                scores,
                preview_limited: preview_limited(&r.report),
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{new_id, Dataset};
    use serde_json::json;

    fn run(report: Value, finished_offset: i64) -> BenchmarkRun {
        BenchmarkRun {
            id: new_id(),
            benchmark_id: "b".into(),
            started_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now() + chrono::Duration::seconds(finished_offset)),
            n_cases: 0,
            mean_score: None,
            pass_rate: None,
            cost_usd: 0.0,
            status: "passed".into(),
            p50_latency_ms: None,
            p95_latency_ms: None,
            total_tokens: None,
            report,
        }
    }

    /// A report the way compare mode writes one: `case` beside `score` on every logged case.
    fn compare_report(target: &str, scored: &[(u32, f64)], dsv: Option<u64>) -> Value {
        let cases: Vec<Value> = scored
            .iter()
            .map(|(c, s)| json!({ "case": c, "score": s }))
            .collect();
        let mut v = json!({ "mode": "compare", "target": target, "cases": cases });
        if let Some(d) = dsv {
            v["dataset_version"] = json!(d);
        }
        v
    }

    /// Cases 1..n, the shape a run with no errored cell produces.
    fn whole(scores: &[f64]) -> Vec<(u32, f64)> {
        scores
            .iter()
            .enumerate()
            .map(|(i, s)| (i as u32 + 1, *s))
            .collect()
    }

    fn cs(pairs: &[(u32, f64)]) -> Vec<CaseScore> {
        pairs.iter().map(|&(c, s)| CaseScore::new(c, s)).collect()
    }

    #[test]
    fn case_scores_needs_every_case_scored_and_identified() {
        assert_eq!(
            case_scores(&compare_report("t", &[(1, 0.5), (2, 0.7)], None)),
            Some(cs(&[(1, 0.5), (2, 0.7)]))
        );
        // A case with no numeric score would misalign the pairing → refuse the whole vector.
        let partial =
            json!({ "cases": [{ "case": 1, "score": 0.5 }, { "case": 2, "pass": true }] });
        assert!(case_scores(&partial).is_none());
        // A case with no `case` index cannot be aligned by identity, and its array position is a
        // rank among judged cases rather than a case number — so it is refused, not guessed at.
        let anonymous = json!({ "cases": [{ "score": 0.5 }, { "score": 0.7 }] });
        assert!(
            case_scores(&anonymous).is_none(),
            "a report with no case identity is not a baseline this test can pair"
        );
        // Reports predating per-case detail, and empty ones, yield nothing.
        assert!(case_scores(&json!({ "mode": "compare" })).is_none());
        assert!(case_scores(&json!({ "cases": [] })).is_none());
    }

    #[test]
    fn picks_the_newest_comparable_run_for_the_same_target() {
        let runs = vec![
            run(compare_report("gpt", &whole(&[0.1, 0.2]), Some(3)), 10),
            run(compare_report("gpt", &whole(&[0.5, 0.6]), Some(3)), 40), // newest for gpt
            run(compare_report("gemini", &whole(&[0.9, 0.9]), Some(3)), 50), // another target
        ];
        let mine = cs(&[(1, 0.0), (2, 0.0)]);
        assert_eq!(
            previous_case_scores(&runs, "gpt", &mine, Some(3)).map(|p| p.scores),
            Some(cs(&[(1, 0.5), (2, 0.6)]))
        );
        assert_eq!(
            previous_case_scores(&runs, "gemini", &mine, Some(3)).map(|p| p.scores),
            Some(cs(&[(1, 0.9), (2, 0.9)]))
        );
        assert!(previous_case_scores(&runs, "claude", &mine, Some(3)).is_none());
    }

    /// The mode/target/dataset-version strictness is unchanged — only the *count* match is gone.
    #[test]
    fn refuses_to_pair_across_a_changed_dataset_version() {
        let runs = vec![run(
            compare_report("gpt", &whole(&[0.4, 0.4]), Some(9)), // wrong dataset
            30,
        )];
        let mine = cs(&[(1, 0.0), (2, 0.0)]);
        assert!(
            previous_case_scores(&runs, "gpt", &mine, Some(3)).is_none(),
            "a different dataset version may never be paired"
        );
        // A legacy run that recorded no dataset version is still usable when the cases line up.
        let legacy = vec![run(compare_report("gpt", &whole(&[0.4, 0.4]), None), 30)];
        assert_eq!(
            previous_case_scores(&legacy, "gpt", &mine, Some(3)).map(|p| p.scores),
            Some(cs(&[(1, 0.4), (2, 0.4)]))
        );
    }

    /// **The count match is gone, and it was never the guard it looked like.** A baseline with three
    /// cases and a run with two used to be refused on the count; a baseline that scored cases 1,2,4
    /// and a run that scored 1,2,3 used to be *accepted* on it and paired case 3 against case 4.
    /// Both are now decided by identity: the first pairs over its overlap, the second over cases 1
    /// and 2 only.
    #[test]
    fn a_different_case_count_is_no_longer_the_test_and_identity_is() {
        let mine = cs(&[(1, 0.0), (2, 0.0)]);
        let longer = vec![run(
            compare_report("gpt", &whole(&[0.5, 0.6, 0.7]), Some(3)),
            40,
        )];
        let p = previous_case_scores(&longer, "gpt", &mine, Some(3)).expect("cases 1,2 are shared");
        assert_eq!(p.scores.len(), 3, "the baseline is returned whole");
        // …and a baseline over a genuinely different set of cases is not a baseline at all.
        let elsewhere = vec![run(
            compare_report("gpt", &[(7, 0.5), (8, 0.6)], Some(3)),
            40,
        )];
        assert!(
            previous_case_scores(&elsewhere, "gpt", &mine, Some(3)).is_none(),
            "no shared case means no baseline, whatever the counts are"
        );
        // The newest run that shares a case wins — an older, fully-overlapping run does not
        // out-rank a newer partial one, but a newer *disjoint* one is skipped rather than chosen.
        let mixed = vec![
            run(compare_report("gpt", &whole(&[0.5, 0.6]), Some(3)), 10),
            run(compare_report("gpt", &[(7, 0.9), (8, 0.9)], Some(3)), 50),
        ];
        assert_eq!(
            previous_case_scores(&mixed, "gpt", &mine, Some(3)).map(|p| p.scores),
            Some(cs(&[(1, 0.5), (2, 0.6)])),
            "a disjoint newer run is not a baseline; the older shared one is"
        );
    }

    /// A baseline whose `cases` array was clipped by `attach_cases` is usable — over the prefix both
    /// runs logged — but only because it comes back FLAGGED. Under the old count match a run bigger
    /// than the preview had no baseline at all.
    #[test]
    fn a_preview_limited_baseline_is_usable_and_says_so() {
        let mut report = compare_report("gpt", &whole(&[0.5, 0.6, 0.7]), Some(3));
        report["cases_truncated"] = json!(true);
        report["cases_total"] = json!(500);
        report["cases_logged"] = json!(3);
        let mine = cs(&[(1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0)]);
        let p = previous_case_scores(&[run(report, 40)], "gpt", &mine, Some(3)).expect("shared");
        assert!(
            p.preview_limited,
            "a systematic prefix must never read as the whole run"
        );
        // An untruncated one carries no such flag — otherwise the caveat would fire on every run.
        let whole_run = vec![run(compare_report("gpt", &whole(&[0.5, 0.6]), Some(3)), 40)];
        assert!(
            !previous_case_scores(&whole_run, "gpt", &mine, Some(3))
                .expect("shared")
                .preview_limited
        );
    }

    /// The guard against a **real** fork, not a hand-written version number (M24).
    ///
    /// Until forking existed, `Dataset::version` was `1` for every dataset that had ever been
    /// created, so this refusal compared 1 with 1 and could never fire — a genuinely different
    /// corpus paired as if it were the same one. This builds the pin the way `bench` builds it, from
    /// a v1 and the v2 a fork produces, and asserts the two do not pair.
    #[test]
    fn a_run_over_a_forked_corpus_does_not_pair_with_its_parents_run() {
        let v1: Dataset =
            serde_json::from_value(json!({ "name": "golden", "version": 1, "frozen": true }))
                .expect("v1");
        let v2: Dataset = serde_json::from_value(json!({
            "name": "golden", "version": 2, "frozen": false, "parent_id": v1.id,
        }))
        .expect("v2");
        assert_eq!(v2.parent_id.as_deref(), Some(v1.id.as_str()));

        let pin_v1 = crate::bench::dataset_pin(None, &v1);
        let pin_v2 = crate::bench::dataset_pin(None, &v2);
        let dsv = |p: &Value| p["dataset_version"].as_u64();
        assert_eq!(dsv(&pin_v1), Some(1));
        assert_eq!(dsv(&pin_v2), Some(2), "a fork moves the pin");

        let mine = cs(&[(1, 0.0), (2, 0.0)]);
        // The v1 run is the only finished baseline on record; a v2 run must refuse to pair with it.
        let history = vec![run(
            compare_report("gpt", &whole(&[0.5, 0.6]), dsv(&pin_v1)),
            40,
        )];
        assert!(
            previous_case_scores(&history, "gpt", &mine, dsv(&pin_v2)).is_none(),
            "a run over the fork must not be paired against its parent's run — the case set changed"
        );
        // …and a second v2 run pairs with the first, which is what makes the fork usable at all.
        let history = vec![run(
            compare_report("gpt", &whole(&[0.5, 0.6]), dsv(&pin_v2)),
            50,
        )];
        assert_eq!(
            previous_case_scores(&history, "gpt", &mine, dsv(&pin_v2)).map(|p| p.scores),
            Some(cs(&[(1, 0.5), (2, 0.6)]))
        );
    }

    #[test]
    fn an_unfinished_run_is_not_a_baseline() {
        let mut r = run(compare_report("gpt", &whole(&[0.5, 0.6]), None), 40);
        r.finished_at = None;
        assert!(previous_case_scores(&[r], "gpt", &cs(&[(1, 0.0), (2, 0.0)]), None).is_none());
    }

    #[test]
    fn a_pairwise_or_rubric_run_is_not_a_compare_baseline() {
        let runs = vec![run(
            json!({ "mode": "pairwise", "target": "gpt", "cases": [{"case":1,"score":0.5}] }),
            40,
        )];
        assert!(previous_case_scores(&runs, "gpt", &cs(&[(1, 0.0)]), None).is_none());
    }
}
