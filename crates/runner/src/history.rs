//! Finding the **previous comparable run** of a benchmark, so this run's verdict can be paired
//! per case instead of compared mean-to-scalar.
//!
//! "Comparable" is deliberately strict. A paired test over case sets that aren't the same cases is
//! worse than no paired test at all — it would silently attribute a dataset change to the model —
//! so a candidate run must match on mode, target, case count, and (when both recorded it, which is
//! every run since candidate generation was pinned) dataset version.

use lighttrack_core::BenchmarkRun;
use serde_json::Value;

/// The per-case scores a compare-mode run report recorded, in case order. `None` when the report
/// predates per-case reporting or any case is missing a numeric score — a partial vector would pair
/// the wrong cases together.
pub(crate) fn case_scores(report: &Value) -> Option<Vec<f64>> {
    let cases = report.get("cases")?.as_array()?;
    if cases.is_empty() {
        return None;
    }
    cases
        .iter()
        .map(|c| c.get("score").and_then(Value::as_f64))
        .collect()
}

/// The dataset version a run was scored over, when it recorded one.
fn dataset_version(report: &Value) -> Option<u64> {
    report.get("dataset_version").and_then(Value::as_u64)
}

/// Per-case scores from the most recent finished run that scored `target` in compare mode over a
/// comparable case set. `n_cases` is this run's case count; `dsv` its dataset version (when known).
///
/// Runs are considered newest-`finished_at`-first. A run still in flight (`finished_at = None`) is
/// skipped: an unfinished run's report is not a baseline.
pub(crate) fn previous_case_scores(
    runs: &[BenchmarkRun],
    target: &str,
    n_cases: usize,
    dsv: Option<u64>,
) -> Option<Vec<f64>> {
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
    candidates
        .iter()
        .rev()
        .find_map(|r| case_scores(&r.report).filter(|s| s.len() == n_cases))
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

    fn compare_report(target: &str, scores: &[f64], dsv: Option<u64>) -> Value {
        let cases: Vec<Value> = scores.iter().map(|s| json!({ "score": s })).collect();
        let mut v = json!({ "mode": "compare", "target": target, "cases": cases });
        if let Some(d) = dsv {
            v["dataset_version"] = json!(d);
        }
        v
    }

    #[test]
    fn case_scores_needs_every_case_scored() {
        assert_eq!(
            case_scores(&compare_report("t", &[0.5, 0.7], None)),
            Some(vec![0.5, 0.7])
        );
        // A case with no numeric score would misalign the pairing → refuse the whole vector.
        let partial = json!({ "cases": [{ "score": 0.5 }, { "pass": true }] });
        assert!(case_scores(&partial).is_none());
        // Reports predating per-case detail, and empty ones, yield nothing.
        assert!(case_scores(&json!({ "mode": "compare" })).is_none());
        assert!(case_scores(&json!({ "cases": [] })).is_none());
    }

    #[test]
    fn picks_the_newest_comparable_run_for_the_same_target() {
        let runs = vec![
            run(compare_report("gpt", &[0.1, 0.2], Some(3)), 10),
            run(compare_report("gpt", &[0.5, 0.6], Some(3)), 40), // newest for gpt
            run(compare_report("gemini", &[0.9, 0.9], Some(3)), 50), // another target
        ];
        assert_eq!(
            previous_case_scores(&runs, "gpt", 2, Some(3)),
            Some(vec![0.5, 0.6])
        );
        assert_eq!(
            previous_case_scores(&runs, "gemini", 2, Some(3)),
            Some(vec![0.9, 0.9])
        );
        assert!(previous_case_scores(&runs, "claude", 2, Some(3)).is_none());
    }

    #[test]
    fn refuses_to_pair_across_a_changed_case_set() {
        let runs = vec![
            run(compare_report("gpt", &[0.5, 0.6, 0.7], Some(3)), 40), // 3 cases, we have 2
            run(compare_report("gpt", &[0.4, 0.4], Some(9)), 30),      // 2 cases, wrong dataset
        ];
        assert!(
            previous_case_scores(&runs, "gpt", 2, Some(3)).is_none(),
            "neither a different case count nor a different dataset version may be paired"
        );
        // A legacy run that recorded no dataset version is still usable when the shape matches.
        let legacy = vec![run(compare_report("gpt", &[0.4, 0.4], None), 30)];
        assert_eq!(
            previous_case_scores(&legacy, "gpt", 2, Some(3)),
            Some(vec![0.4, 0.4])
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

        // The v1 run is the only finished baseline on record; a v2 run must refuse to pair with it.
        let history = vec![run(compare_report("gpt", &[0.5, 0.6], dsv(&pin_v1)), 40)];
        assert!(
            previous_case_scores(&history, "gpt", 2, dsv(&pin_v2)).is_none(),
            "a run over the fork must not be paired against its parent's run — the case set changed"
        );
        // …and a second v2 run pairs with the first, which is what makes the fork usable at all.
        let history = vec![run(compare_report("gpt", &[0.5, 0.6], dsv(&pin_v2)), 50)];
        assert_eq!(
            previous_case_scores(&history, "gpt", 2, dsv(&pin_v2)),
            Some(vec![0.5, 0.6])
        );
    }

    #[test]
    fn an_unfinished_run_is_not_a_baseline() {
        let mut r = run(compare_report("gpt", &[0.5, 0.6], None), 40);
        r.finished_at = None;
        assert!(previous_case_scores(&[r], "gpt", 2, None).is_none());
    }

    #[test]
    fn a_pairwise_or_rubric_run_is_not_a_compare_baseline() {
        let runs = vec![run(
            json!({ "mode": "pairwise", "target": "gpt", "cases": [{"score":0.5}] }),
            40,
        )];
        assert!(previous_case_scores(&runs, "gpt", 1, None).is_none());
    }
}
