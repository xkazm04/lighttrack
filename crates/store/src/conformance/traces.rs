//! `Surface::Traces`: the listing rollup, the bounded detail read, and the verdicts attached to a
//! trace — held to the SQLite reference's semantics on every backend that declares the surface.

use chrono::Utc;

use lighttrack_core::{new_id, Score, ScoreKind, Status};

use super::fixtures::sample_event;
use crate::{Result, Store, TraceFilter};

/// The trace surface: the listing rollup, the bounded detail read, and the verdicts attached to a
/// trace — held to the SQLite reference's semantics on every backend that claims to serve them.
///
/// Two things this must catch, both of which the trait *defaults* produce:
/// * a backend that inherits the defaults while declaring [`Store::serves_traces`] — every
///   assertion below fails immediately (the defaults refuse outright, and `list_traces_filtered`'s
///   default ignores the filter and mints no cursor),
/// * a backend that answers a trace read with an empty page instead of refusing — the
///   `serves_traces() == false` branch asserts an explicit [`StoreError::Unsupported`], so "not
///   implemented" can never read as "you have no traces".
///
/// The **cross-project collision** case is the tenancy property: `trace_id` is caller-supplied, so
/// two projects can carry the same id and neither may see the other's spans, verdicts, or cost.
pub(super) fn traces(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let other = new_id();
    let tid = format!("t-{}", new_id());

    // One trace of three spans, 100ms apart, each with 42ms of latency; the last one errored.
    // Inserted out of chronological order so nothing can pass by accident of insertion order.
    let t0 = Utc::now();
    let span = |model: &str, offset_ms: i64, cost: f64, status: Status| {
        let mut e = sample_event(&pid, model, 10, 5, cost);
        e.trace_id = Some(tid.clone());
        e.ts = t0 + chrono::Duration::milliseconds(offset_ms);
        e.status = status;
        e
    };
    store.insert_event(&span("m-second", 100, 0.2, Status::Success))?;
    store.insert_event(&span("m-first", 200, 0.4, Status::Error))?;
    store.insert_event(&span("m-first", 0, 0.1, Status::Success))?;
    // A second, cheaper, single-span trace in the same project (older, so it sorts second).
    let tid2 = format!("t-{}", new_id());
    let mut lone = sample_event(&pid, "m-first", 1, 1, 0.05);
    lone.trace_id = Some(tid2.clone());
    lone.ts = t0 - chrono::Duration::seconds(30);
    store.insert_event(&lone)?;
    // The collision: another project reusing the *same* trace id, with its own cost and model.
    let mut intruder = sample_event(&other, "m-intruder", 999, 999, 100.0);
    intruder.trace_id = Some(tid.clone());
    intruder.ts = t0 + chrono::Duration::milliseconds(50);
    store.insert_event(&intruder)?;

    // --- listing ---
    let listed = store.list_traces(Some(&pid), 50)?;
    assert_eq!(
        listed.len(),
        2,
        "both of this project's traces roll up (the default returns none)"
    );
    assert_eq!(listed[0].trace_id, tid, "newest-ended first");
    let a = listed[0].clone();
    assert_eq!(a.project_id, pid);
    assert_eq!(
        a.spans, 3,
        "the other project's colliding span is NOT merged in"
    );
    assert!(
        (a.cost_usd - 0.7).abs() < 1e-9,
        "cost sums this project's spans only: {}",
        a.cost_usd
    );
    assert_eq!(a.errors, 1);
    assert_eq!(a.status, "error", "a trace is `error` iff any span errored");
    assert_eq!(a.input_tokens, 30);
    assert_eq!(a.total_tokens, 45);
    // The last span's own latency counts: 200ms of spread + 42ms of trailing compute — NOT
    // MAX(ts) - MIN(ts), the start-to-start number the list used to report.
    assert_eq!(
        a.duration_ms, 242,
        "summary duration includes the last span's latency"
    );
    assert_eq!(
        a.models,
        vec!["m-first".to_string(), "m-second".to_string()],
        "distinct models in first-seen order, and never the other project's model"
    );

    // --- filters + keyset paging ---
    let errs = store.list_traces_filtered(
        Some(&pid),
        &TraceFilter {
            status: Some("error".into()),
            ..Default::default()
        },
        50,
    )?;
    assert_eq!(
        errs.traces.len(),
        1,
        "status filter keeps only the errored trace"
    );
    assert_eq!(errs.traces[0].trace_id, tid);
    let ok = store.list_traces_filtered(
        Some(&pid),
        &TraceFilter {
            status: Some("success".into()),
            ..Default::default()
        },
        50,
    )?;
    assert_eq!(
        ok.traces.len(),
        1,
        "…and its complement keeps only the clean one"
    );
    assert_eq!(ok.traces[0].trace_id, tid2);
    let dear = store.list_traces_filtered(
        Some(&pid),
        &TraceFilter {
            min_cost: Some(0.5),
            ..Default::default()
        },
        50,
    )?;
    assert_eq!(
        dear.traces.len(),
        1,
        "min_cost is an aggregate predicate over the trace's spans"
    );
    assert_eq!(dear.traces[0].trace_id, tid);
    let windowed = store.list_traces_filtered(
        Some(&pid),
        &TraceFilter {
            since: Some(t0 - chrono::Duration::seconds(5)),
            ..Default::default()
        },
        50,
    )?;
    assert_eq!(
        windowed.traces.len(),
        1,
        "`since` excludes the trace that ended before it"
    );

    let page1 = store.list_traces_filtered(Some(&pid), &TraceFilter::default(), 1)?;
    assert_eq!(page1.traces.len(), 1, "the page fills to the limit");
    let cursor = page1
        .next_cursor
        .clone()
        .expect("more traces remain -> next_cursor is minted");
    let page2 = store.list_traces_filtered(
        Some(&pid),
        &TraceFilter {
            cursor: Some(cursor),
            ..Default::default()
        },
        1,
    )?;
    assert_eq!(
        page2.traces.len(),
        1,
        "the (ended, trace_id) keyset continues, not restarts"
    );
    assert_ne!(
        page1.traces[0].trace_id, page2.traces[0].trace_id,
        "no trace served twice"
    );
    assert!(
        page2.next_cursor.is_none(),
        "exhausted -> no further cursor"
    );

    // --- detail ---
    let evs = store.list_trace_events(Some(&pid), &tid, 50)?;
    assert_eq!(evs.total, 3);
    assert_eq!(evs.events.len(), 3);
    assert!(
        evs.events.iter().all(|e| e.project_id == pid),
        "another project's span is invisible"
    );
    assert!(
        evs.events.windows(2).all(|w| w[0].ts <= w[1].ts),
        "oldest first"
    );
    // The cap keeps the trace's head and reports the true span count, so a clipped read says so.
    let clipped = store.list_trace_events(Some(&pid), &tid, 2)?;
    assert_eq!(
        clipped.events.len(),
        2,
        "at most max_spans events come back"
    );
    assert_eq!(
        clipped.total, 3,
        "…and `total` is still the trace's real span count"
    );
    assert_eq!(
        clipped.events[0].ts, evs.events[0].ts,
        "the retained window is the trace's head"
    );

    let trace = store
        .get_trace(Some(&pid), &tid, 50)?
        .expect("get_trace Some");
    assert_eq!(trace.trace_id, tid);
    assert_eq!(trace.project_id, pid);
    assert_eq!(trace.totals.spans, 3);
    assert!(!trace.spans_truncated);
    assert_eq!(
        trace.duration_ms, a.duration_ms,
        "list and detail report the ONE duration rule (TraceShape), not two"
    );
    assert_eq!(trace.status, a.status, "…and the one status rule");
    assert_eq!(trace.models, a.models, "…and the same model ordering");
    let short = store
        .get_trace(Some(&pid), &tid, 2)?
        .expect("clipped get_trace Some");
    assert!(short.spans_truncated, "a clipped trace must say so");
    assert_eq!((short.spans_total, short.spans_logged), (3, 2));

    // Tenancy: the other project's rollup of the same id sees only its own span, and a project that
    // has no span under this id gets None (404-shaped: invisible, not forbidden).
    let theirs = store
        .get_trace(Some(&other), &tid, 50)?
        .expect("the colliding trace exists there");
    assert_eq!(
        theirs.totals.spans, 1,
        "the collision resolves per project, both ways"
    );
    assert!((theirs.totals.cost_usd - 100.0).abs() < 1e-9);
    assert!(
        store.get_trace(Some(&new_id()), &tid, 50)?.is_none(),
        "invisible to a third project"
    );
    assert!(
        store.get_trace(Some(&pid), &new_id(), 50)?.is_none(),
        "unknown trace id -> None"
    );

    // --- verdicts attached to a trace ---
    let root = evs.events[0].id.clone();
    let verdict = |project: &str, event_id: &str, rubric: &str| Score {
        id: new_id(),
        project_id: project.into(),
        event_id: Some(event_id.into()),
        rubric: rubric.into(),
        rubric_id: None,
        kind: ScoreKind::Trace,
        value: 0.9,
        max: 1.0,
        pass: Some(true),
        reasoning: Some("whole-trace verdict".into()),
        detail: None,
        run_id: None,
        case_index: None,
        scored_by: "judge".into(),
        cost_usd: Some(0.01),
        created_at: Utc::now(),
    };
    store.insert_score(&verdict(&pid, &root, "whole-trace"))?;
    store.insert_score(&verdict(&other, &intruder.id, "not-yours"))?;
    let got = store.list_trace_scores(Some(&pid), &tid)?;
    assert_eq!(
        got.len(),
        1,
        "a score reaches its trace through its event_id"
    );
    assert_eq!(got[0].rubric, "whole-trace");
    assert!(
        !got.iter().any(|s| s.rubric == "not-yours"),
        "a verdict on the colliding trace in another project never surfaces here"
    );
    assert_eq!(
        store.list_trace_scores(Some(&other), &tid)?.len(),
        1,
        "…and that project sees exactly its own"
    );
    Ok(())
}
