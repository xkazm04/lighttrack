//! The seven substantive acceptance cases for the per-dimension regression table. `diff` is
//! I/O-free, so every one of them is a fold over hand-built verdicts — no router, no store, no judge.

use lighttrack_core::{Score, ScoreDetail, ScoreDim};
use serde_json::json;

use super::*;

/// A stored verdict carrying only what these cases are about.
fn score(case: u32, pass: Option<bool>, detail: ScoreDetail) -> Score {
    let mut s: Score = serde_json::from_value(json!({
        "rubric": "r", "value": 0.0, "max": 1.0, "scored_by": "haiku",
    }))
    .expect("a Score built from its own defaults");
    s.case_index = Some(case);
    s.pass = pass;
    s.detail = Some(detail);
    s
}

fn dim(key: &str, value: f64) -> ScoreDim {
    ScoreDim {
        key: key.into(),
        value,
        weight: 1.0,
        ..Default::default()
    }
}

fn floored(key: &str, value: f64, floor: f64) -> ScoreDim {
    ScoreDim {
        floor: Some(floor),
        floor_hit: value < floor,
        ..dim(key, value)
    }
}

fn detail(dims: Vec<ScoreDim>) -> ScoreDetail {
    ScoreDetail {
        dimensions: dims,
        ..Default::default()
    }
}

fn row<'a>(d: &'a RegressionDiff, key: &str) -> &'a DimDelta {
    d.dimensions
        .iter()
        .find(|r| r.key == key)
        .unwrap_or_else(|| panic!("no '{key}' row in {:?}", d.dimensions))
}

fn family<'a>(rows: &'a [FamilyCount], name: &str) -> &'a FamilyCount {
    rows.iter()
        .find(|f| f.family == name)
        .unwrap_or_else(|| panic!("no '{name}' family"))
}

/// Case 1 — one row per dimension key with both means, the delta, both `n`s and both floor-hit
/// counts, **ordered by delta ascending** so the worst-dropping dimension is the first thing read.
#[test]
fn every_dimension_gets_a_row_worst_drop_first() {
    let before = vec![
        score(
            1,
            Some(true),
            detail(vec![dim("accuracy", 1.0), floored("safety", 1.0, 0.5)]),
        ),
        score(
            2,
            Some(true),
            detail(vec![dim("accuracy", 0.8), floored("safety", 1.0, 0.5)]),
        ),
    ];
    let after = vec![
        score(
            1,
            Some(true),
            detail(vec![dim("accuracy", 0.9), floored("safety", 0.2, 0.5)]),
        ),
        score(
            2,
            Some(true),
            detail(vec![dim("accuracy", 0.9), floored("safety", 0.4, 0.5)]),
        ),
    ];

    let d = diff(&before, &after);
    assert_eq!(d.status, "ok");
    assert_eq!(d.dimensions.len(), 2);
    // Safety fell 0.7; accuracy moved +0.0. The worst drop is first.
    assert_eq!(d.dimensions[0].key, "safety");

    let safety = row(&d, "safety");
    assert_eq!(safety.status, "compared");
    assert_eq!(safety.mean_before, Some(1.0));
    assert_eq!(safety.mean_after, Some(0.30000000000000004));
    assert!((safety.delta.expect("delta") + 0.7).abs() < 1e-9);
    assert_eq!((safety.n_before, safety.n_after), (2, 2));
    assert_eq!(
        (safety.floor_hits_before, safety.floor_hits_after),
        (0, 2),
        "both floor-hit counts are on the row"
    );

    let accuracy = row(&d, "accuracy");
    assert_eq!(accuracy.mean_before, Some(0.9));
    assert_eq!(accuracy.mean_after, Some(0.9));
    assert_eq!(accuracy.delta, Some(0.0));
}

/// Case 2 — a case that passed before and fails now appears under `flipped` with its `case_index`
/// and the **stored reasonings** of the worst-dropping dimension, so the operator reads the judge's
/// own words rather than a number.
#[test]
fn a_flipped_case_carries_the_judges_words_for_the_dimension_that_moved() {
    let before = vec![score(7, Some(true), detail(vec![dim("safety", 1.0)]))];
    let after = vec![score(
        7,
        Some(false),
        detail(vec![ScoreDim {
            reasoning: vec!["named a real CVE but invented the patch version".into()],
            ..dim("safety", 0.1)
        }]),
    )];

    let d = diff(&before, &after);
    assert_eq!(d.flipped.len(), 1);
    let f = &d.flipped[0];
    assert_eq!(f.case_index, 7);
    assert_eq!(f.dimension.as_deref(), Some("safety"));
    assert_eq!(
        f.reasonings,
        vec!["named a real CVE but invented the patch version".to_string()]
    );

    // A case that passed both times is not a flip, and a case present on one side only is counted
    // out loud instead of averaged across two different populations.
    let d = diff(
        &[score(1, Some(true), detail(vec![dim("safety", 1.0)]))],
        &[
            score(1, Some(true), detail(vec![dim("safety", 1.0)])),
            score(2, Some(false), detail(vec![dim("safety", 0.0)])),
        ],
    );
    assert!(d.flipped.is_empty());
    assert_eq!((d.unpaired_before, d.unpaired_after), (0, 1));
}

/// Case 3 — failure-family counts beside the means, each with the denominator of cases that could
/// have tripped it. **A family whose denominator is 0 renders `not_measured`, never `0`**: a
/// detector that cannot fire and a detector that found nothing call for opposite actions.
#[test]
fn every_family_carries_its_denominator_and_a_zero_denominator_is_not_measured() {
    let shaky = score(
        1,
        Some(false),
        ScoreDetail {
            dimensions: vec![floored("safety", 0.1, 0.5)],
            agreement: Some(0.4),
            samples_requested: Some(3),
            samples_parsed: Some(2),
            parse_failures: Some(1),
            injection_suspected: Some(true),
            ..Default::default()
        },
    );
    let clean = score(
        2,
        Some(true),
        ScoreDetail {
            dimensions: vec![floored("safety", 0.9, 0.5)],
            agreement: Some(1.0),
            samples_requested: Some(3),
            samples_parsed: Some(3),
            parse_failures: Some(0),
            injection_suspected: Some(false),
            ..Default::default()
        },
    );
    let after = vec![shaky, clean.clone()];
    let d = diff(std::slice::from_ref(&clean), &after);

    for (name, cases, denominator) in [
        ("parse_failures", 1, 2),
        ("low_agreement", 1, 2),
        ("injection_suspected", 1, 2),
        ("floor_hit", 1, 2),
    ] {
        let f = family(&d.families_after, name);
        assert_eq!(f.status, "measured", "{name}");
        assert_eq!((f.cases, f.denominator), (cases, denominator), "{name}");
    }

    // The same four families on a run whose verdicts carry none of the signals: every denominator
    // is 0, so every one reads `not_measured`.
    let bare = diff(
        &[score(1, Some(true), detail(vec![dim("accuracy", 1.0)]))],
        &[score(1, Some(true), detail(vec![dim("accuracy", 1.0)]))],
    );
    for name in [
        "parse_failures",
        "low_agreement",
        "injection_suspected",
        "floor_hit",
    ] {
        let f = family(&bare.families_after, name);
        assert_eq!(f.status, "not_measured", "{name} has no instrument here");
        assert_eq!((f.cases, f.denominator), (0, 0), "{name}");
    }
}

/// Case 5 — a run holding only freeform verdicts (no dimensions) answers `no_dimensions` and
/// **names the freeform contract as the reason**. An empty dimension array would read as "nothing
/// regressed", which is the one answer this surface must never give by accident.
#[test]
fn a_freeform_only_run_says_no_dimensions_and_why() {
    let before = vec![score(1, Some(true), ScoreDetail::default())];
    let after = vec![score(
        1,
        Some(false),
        ScoreDetail {
            notes: vec!["the answer drifted off-topic".into()],
            agreement: Some(0.9),
            ..Default::default()
        },
    )];

    let d = diff(&before, &after);
    assert_eq!(d.status, "no_dimensions");
    assert!(d.dimensions.is_empty());
    let why = d.reason.as_deref().expect("a named reason");
    assert!(why.contains("freeform"), "{why}");
    assert!(why.contains("rubric_id"), "the repair is named: {why}");
    // The families still report — a freeform run's reliability counters are real.
    assert_eq!(family(&d.families_after, "low_agreement").denominator, 1);
}

/// Case 6 — a dimension voided on one side and scored on the other is `coverage_changed` with both
/// counts and **no delta**. `judge::aggregate` stores `0.0` as the placeholder for a voided
/// dimension, so a naive subtraction would publish a −1.0 collapse that never happened.
#[test]
fn a_dimension_voided_on_one_side_is_coverage_changed_not_a_delta() {
    let before = vec![score(
        1,
        Some(true),
        detail(vec![dim("accuracy", 1.0), dim("exec", 1.0)]),
    )];
    let after = vec![score(
        1,
        Some(true),
        detail(vec![
            dim("accuracy", 1.0),
            ScoreDim {
                voided: true,
                // The placeholder `aggregate` stores. A delta against it would read −1.0.
                ..dim("exec", 0.0)
            },
        ]),
    )];

    let d = diff(&before, &after);
    let exec = row(&d, "exec");
    assert_eq!(exec.status, "coverage_changed");
    assert_eq!(exec.delta, None, "no delta against a placeholder");
    assert_eq!(exec.mean_before, Some(1.0));
    assert_eq!(exec.mean_after, None);
    assert_eq!((exec.n_before, exec.n_after), (1, 0));
    assert_eq!((exec.voided_before, exec.voided_after), (0, 1));
    // …and it sorts after every compared row rather than claiming a place in the ranking.
    assert_eq!(d.dimensions.last().expect("rows").key, "exec");

    // A dimension that only one run measured at all is the same fact.
    let d = diff(
        &[score(1, Some(true), detail(vec![dim("accuracy", 1.0)]))],
        &[score(
            1,
            Some(true),
            detail(vec![dim("accuracy", 1.0), dim("tone", 0.5)]),
        )],
    );
    assert_eq!(row(&d, "tone").status, "coverage_changed");
    assert_eq!(row(&d, "tone").delta, None);
}

/// A compare run writes several cells per case. "One target passed" is not the case passing, so the
/// case verdict is the conjunction over the observations that carry one.
#[test]
fn a_case_passes_only_when_every_observation_that_voted_passed() {
    let before = vec![
        score(1, Some(true), detail(vec![dim("accuracy", 1.0)])),
        score(1, Some(true), detail(vec![dim("accuracy", 0.9)])),
    ];
    let after = vec![
        score(1, Some(true), detail(vec![dim("accuracy", 0.9)])),
        score(1, Some(false), detail(vec![dim("accuracy", 0.2)])),
    ];
    let d = diff(&before, &after);
    assert_eq!(d.flipped.len(), 1, "one cell failing fails the case");
    assert_eq!(d.flipped[0].case_index, 1);

    // A verdict with no `pass` at all votes on nothing: it cannot manufacture a flip.
    let d = diff(
        &[score(1, None, detail(vec![dim("accuracy", 1.0)]))],
        &[score(1, Some(false), detail(vec![dim("accuracy", 0.0)]))],
    );
    assert!(d.flipped.is_empty());
}
