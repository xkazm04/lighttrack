//! Unit tests for the post-merge narrowing pipeline. Out of line so `leaderboard.rs` stays under
//! the file-size ceiling; the ORDER these pin (source floor before every user filter) is the privacy
//! property, so they live beside the function rather than in an integration test.

use super::*;
use lighttrack_core::{Coverage, DEFAULT_LOW_CONFIDENCE_CASES};

fn params() -> LeaderboardParams {
    LeaderboardParams {
        task_type: None,
        provider: None,
        judge: None,
        determinism: None,
        frozen_dataset: None,
        significance_tested: None,
        rubric: None,
        fresh_days: None,
    }
}

fn entry(contrib: &str, model: &str, fp: Option<&str>, age_days: i64) -> CollectiveEntry {
    CollectiveEntry {
        contributor_id: contrib.into(),
        provider: "anthropic".into(),
        model: model.into(),
        task_type: "qa".into(),
        quality: 0.8,
        pass_rate: 0.8,
        avg_cost_usd: 0.003,
        p50_latency_ms: None,
        p95_latency_ms: None,
        n_runs: 1,
        n_cases: 50,
        quality_variance: None,
        judge_provider: None,
        rubric_fingerprint: fp.map(str::to_string),
        determinism: None,
        frozen_dataset: Coverage::Unknown,
        significance_tested: Coverage::Unknown,
        received_at: Utc::now() - Duration::days(age_days),
    }
}

fn narrowed(entries: &[CollectiveEntry], q: &LeaderboardParams, k: u32) -> Narrowed {
    let rows = merge_leaderboard(entries, DEFAULT_LOW_CONFIDENCE_CASES);
    narrow(rows, entries, q, k, Utc::now())
}

#[test]
fn fresh_days_keeps_rows_with_any_recent_source_and_recounts() {
    let entries = [
        entry("a", "haiku", None, 3),
        entry("b", "haiku", None, 200),
        entry("a", "sonnet", None, 400),
        entry("b", "sonnet", None, 400),
    ];
    let mut q = params();
    q.fresh_days = Some(30);
    let n = narrowed(&entries, &q, 2);
    assert_eq!(n.rows.len(), 1, "only the row with a 3-day source survives");
    assert_eq!(n.rows[0].model, "haiku");
    assert_eq!(n.rows[0].n_contributors, 2, "the stale source still counts");
    assert_eq!(n.contributors, 2, "counts recomputed over survivors");
    assert_eq!(n.n_models, 1);
    assert_eq!(n.as_of.as_deref(), n.rows[0].newest_at.as_deref());
}

#[test]
fn a_freshness_filter_cannot_strip_a_row_to_a_lone_source() {
    // Single-source rows are held back by the floor BEFORE the freshness filter runs, so the
    // filter can never be the thing that publishes one.
    let entries = [entry("a", "solo", None, 1), entry("b", "duo", None, 1)];
    let mut q = params();
    q.fresh_days = Some(30);
    let n = narrowed(&entries, &q, 2);
    assert_eq!(n.held_back, 2, "both rows are single-source");
    assert!(n.rows.is_empty());
    assert_eq!(n.contributors, 0);
    assert!(n.as_of.is_none(), "an empty board owns no date");
}

#[test]
fn rubric_filter_selects_commensurable_rows_after_the_source_floor() {
    let entries = [
        entry("a", "haiku", Some("ab12"), 1),
        entry("b", "haiku", Some("cd34"), 1),
        entry("a", "sonnet", Some("cd34"), 1),
        entry("b", "sonnet", Some("cd34"), 1),
        entry("a", "solo", Some("ab12"), 1),
    ];
    let mut q = params();
    q.rubric = Some("ab12".into());
    let n = narrowed(&entries, &q, 2);
    let models: Vec<&str> = n.rows.iter().map(|r| r.model.as_str()).collect();
    assert_eq!(
        models,
        ["haiku"],
        "sonnet never used ab12; solo is held back"
    );
    assert_eq!(n.held_back, 1);
    assert_eq!(
        n.rows[0].mixed_rubrics,
        Some(2),
        "disclosed, still filtered"
    );

    q.rubric = Some("nope".into());
    assert!(narrowed(&entries, &q, 2).rows.is_empty());
}

#[test]
fn as_of_is_the_newest_visible_evidence() {
    let entries = [
        entry("a", "haiku", None, 5),
        entry("b", "haiku", None, 300),
        entry("a", "sonnet", None, 300),
        entry("b", "sonnet", None, 300),
    ];
    let n = narrowed(&entries, &params(), 2);
    assert_eq!(n.rows.len(), 2);
    assert_eq!(n.as_of, Some(day_stamp(Utc::now() - Duration::days(5))));
}
