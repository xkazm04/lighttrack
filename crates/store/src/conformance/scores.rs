//! Verdict storage: round-trips, the unscored-events queue, and one benchmark run's cases.

use chrono::Utc;

use lighttrack_core::{new_id, Score, ScoreDetail, ScoreDim, ScoreKind};

use super::fixtures::sample_event;
use crate::Scope;
use crate::{Result, ScoreFilter, Store};

pub(super) fn scores(store: &dyn Store, pid: &str) -> Result<()> {
    let s = Score {
        id: new_id(),
        project_id: pid.into(),
        event_id: None,
        rubric: "correctness".into(),
        rubric_id: Some("rub-conformance".into()),
        kind: ScoreKind::Rubric,
        value: 0.9,
        max: 1.0,
        pass: Some(true),
        reasoning: Some("ok".into()),
        detail: None,
        run_id: None,
        case_index: None,
        scored_by: "judge".into(),
        cost_usd: Some(0.01),
        created_at: Utc::now(),
    };
    store.insert_score(&s)?;
    let listed = store.list_scores(Scope::Project(pid), 10)?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].scored_by, "judge");
    assert_eq!(listed[0].pass, Some(true));

    // Unscored anti-join (online scorer work list). Insert two events, score exactly one of them, and
    // assert the scoped `scored_event_ids` / `list_unscored_events` see the one and only the one — the
    // guarantee the old top-1000 client anti-join lost once a project passed 1000 scores.
    let scored_ev = sample_event(pid, "claude-haiku-4-5", 1, 1, 0.0);
    let unscored_ev = sample_event(pid, "claude-haiku-4-5", 1, 1, 0.0);
    store.insert_event(&scored_ev)?;
    store.insert_event(&unscored_ev)?;
    let mut sc = s.clone();
    sc.id = new_id();
    sc.event_id = Some(scored_ev.id.clone());
    store.insert_score(&sc)?;

    let scored_set = store.scored_event_ids(
        Scope::Operator,
        &[scored_ev.id.clone(), unscored_ev.id.clone()],
    )?;
    assert_eq!(
        scored_set,
        vec![scored_ev.id.clone()],
        "only the scored event id comes back"
    );
    assert!(
        store.scored_event_ids(Scope::Operator, &[])?.is_empty(),
        "empty input -> empty output"
    );

    let unscored = store.list_unscored_events(Scope::Project(pid), None, 50)?;
    assert!(
        unscored.iter().any(|e| e.id == unscored_ev.id),
        "unscored event is in the work list"
    );
    assert!(
        !unscored.iter().any(|e| e.id == scored_ev.id),
        "scored event is excluded from the work list",
    );
    run_scoped_cases(store, pid)?;
    Ok(())
}

/// Run-scoped case results: a benchmark case knows which run produced it, carries the judge's
/// structured provenance, and "every case result for run X" is one ordered query. Pinned here because
/// a backend that quietly dropped `run_id`/`detail` would still pass every scalar assertion above —
/// and would answer "why did run 47 fail?" with an empty list that reads like "nothing went wrong".
fn run_scoped_cases(store: &dyn Store, pid: &str) -> Result<()> {
    let run_id = new_id();
    let other_run = new_id();
    let detail = ScoreDetail {
        dimensions: vec![ScoreDim {
            key: "safety".into(),
            value: 0.25,
            weight: 1.0,
            floor: Some(0.5),
            floor_hit: true,
            reasoning: vec!["unsafe advice".into()],
            ..Default::default()
        }],
        agreement: Some(0.75),
        samples_requested: Some(3),
        samples_parsed: Some(2),
        parse_failures: Some(1),
        injection_suspected: Some(false),
        determinism: Some("exact".into()),
        // How mangled the judged evidence already was. Asserted through the whole-detail equality
        // below: a backend that dropped it would answer "nothing was rewritten" about text the
        // ingest scrub had rewritten, which is the exact claim M9 exists to stop being free.
        evidence_redacted_spans: Some(2),
        ..Default::default()
    };
    let case = |run: &str, idx: Option<u32>, value: f64| Score {
        id: new_id(),
        project_id: pid.into(),
        event_id: None,
        rubric: "bench:conformance".into(),
        rubric_id: Some("rub-bench".into()),
        kind: ScoreKind::BenchCase,
        value,
        max: 1.0,
        pass: Some(value >= 0.5),
        reasoning: Some("safety: unsafe advice".into()),
        detail: Some(detail.clone()),
        run_id: Some(run.into()),
        case_index: idx,
        scored_by: "judge".into(),
        cost_usd: Some(0.002),
        created_at: Utc::now(),
    };
    // Inserted out of case order, plus one unindexed case and one belonging to a different run.
    store.insert_score(&case(&run_id, Some(2), 0.4))?;
    store.insert_score(&case(&run_id, None, 0.6))?;
    store.insert_score(&case(&run_id, Some(1), 0.9))?;
    store.insert_score(&case(&other_run, Some(1), 0.1))?;

    let cases = store.list_run_scores(&run_id, Scope::Project(pid), 100)?;
    assert_eq!(
        cases.len(),
        3,
        "exactly this run's cases, never another run's"
    );
    assert_eq!(
        cases.iter().map(|c| c.case_index).collect::<Vec<_>>(),
        vec![Some(1), Some(2), None],
        "case order, with unindexed cases last on every backend"
    );
    assert_eq!(
        cases[0].run_id.as_deref(),
        Some(run_id.as_str()),
        "run scoping round-trips"
    );
    assert_eq!(
        cases[0].detail.as_ref(),
        Some(&detail),
        "the per-case provenance rides the case instead of being dropped"
    );
    // The typed identity round-trips. A backend that dropped it would leave every verdict looking
    // like the untyped `freeform` default, which is the state this replaces.
    assert_eq!(cases[0].kind, ScoreKind::BenchCase, "kind round-trip");
    assert_eq!(
        cases[0].rubric_id.as_deref(),
        Some("rub-bench"),
        "rubric_id round-trip"
    );

    // Authorization scope is applied in the query, not by the caller.
    assert!(
        store
            .list_run_scores(&run_id, Scope::Project(&new_id()), 100)?
            .is_empty(),
        "another project's key sees none of this run's cases"
    );
    assert!(
        store
            .list_run_scores(&new_id(), Scope::Operator, 100)?
            .is_empty(),
        "unknown run -> no cases"
    );
    assert_eq!(
        store.list_run_scores(&run_id, Scope::Operator, 2)?.len(),
        2,
        "limit is honored"
    );
    Ok(())
}

/// `Surface::ScoreFilters`: narrowing verdicts by rubric and kind.
///
/// The bar is that the filters actually filter. A backend that ignored them would answer
/// `kind=bench_case` with every score in the project — a page that looks authoritative and is a
/// different question's answer, which is the failure this trait's whole default policy refuses.
pub(super) fn score_filters(store: &dyn Store) -> Result<()> {
    let scope = new_id();
    let mk = |rubric: &str, rubric_id: Option<&str>, kind: ScoreKind| Score {
        id: new_id(),
        project_id: scope.clone(),
        event_id: None,
        rubric: rubric.into(),
        rubric_id: rubric_id.map(str::to_string),
        kind,
        value: 0.5,
        max: 1.0,
        pass: None,
        reasoning: None,
        detail: None,
        run_id: None,
        case_index: None,
        scored_by: "judge".into(),
        cost_usd: None,
        created_at: Utc::now(),
    };
    store.insert_score(&mk("quality", Some("rub-a"), ScoreKind::Rubric))?;
    store.insert_score(&mk("quality", Some("rub-a"), ScoreKind::Rubric))?;
    store.insert_score(&mk("bench:q#case1", Some("rub-a"), ScoreKind::BenchCase))?;
    store.insert_score(&mk("adhoc", None, ScoreKind::Freeform))?;
    store.insert_score(&mk("other", Some("rub-b"), ScoreKind::Rubric))?;

    let by = |f: ScoreFilter| store.list_scores_filtered(Scope::Project(&scope), &f, 100);

    let rub_a = by(ScoreFilter {
        rubric_id: Some("rub-a".into()),
        kind: None,
    })?;
    assert_eq!(rub_a.len(), 3, "every verdict citing rub-a, and only those");
    assert!(rub_a
        .iter()
        .all(|s| s.rubric_id.as_deref() == Some("rub-a")));

    let bench = by(ScoreFilter {
        rubric_id: None,
        kind: Some(ScoreKind::BenchCase.as_str().into()),
    })?;
    assert_eq!(
        bench.len(),
        1,
        "the kind predicate narrows (an unfiltered listing would return 5)"
    );
    assert_eq!(bench[0].kind, ScoreKind::BenchCase);

    // AND, not OR.
    let both = by(ScoreFilter {
        rubric_id: Some("rub-a".into()),
        kind: Some(ScoreKind::Rubric.as_str().into()),
    })?;
    assert_eq!(both.len(), 2, "rubric_id AND kind");

    // Project scoping still applies to a filtered read - a filter must never widen a query.
    let elsewhere = store.list_scores_filtered(
        Scope::Project(&new_id()),
        &ScoreFilter {
            rubric_id: Some("rub-a".into()),
            kind: None,
        },
        100,
    )?;
    assert!(
        elsewhere.is_empty(),
        "another project's key sees none of these verdicts"
    );

    // A kind nobody wrote is an empty page - not an error, and not everything.
    let none = by(ScoreFilter {
        rubric_id: None,
        kind: Some(ScoreKind::Calibration.as_str().into()),
    })?;
    assert!(none.is_empty());
    Ok(())
}
