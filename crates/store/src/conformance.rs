//! Backend-agnostic conformance suite: exercises the full [`Store`] trait and asserts round-trips,
//! so SQLite, Postgres, and Firestore can be held to identical behavior.
//!
//! Each backend crate has an integration test that constructs its store and calls [`run`]. The
//! SQLite (in-memory) test runs in CI always; the Postgres / Firestore tests run only when a test
//! env var points at one. Safe against a **non-empty** database: everything is scoped to a fresh
//! unique project + unique ids, and the inherently-global checks (prices, the job claim) are tolerant.

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{
    compute_margin, new_id, ApiKey, Benchmark, BenchmarkCase, BenchmarkRun, Dataset, DatasetItem,
    DimensionCheck, DimensionKind, Job, JobCancel, LimitAction, LimitMetric, LimitRule, LimitScope,
    LimitWindow, LlmEvent, MarginDimension, ModelPriceRow, Operation, Project, Provider, Redaction,
    RelayOutcome, RelayTask, RevenueEvent, RevenueKind, Rubric, RubricDimension, Score,
    ScoreDetail, ScoreDim, Status, TokenUsage,
};

use crate::{Admission, EventFilter, Result, Store, StoreError, TraceFilter};

/// Run the full conformance suite against `store` (assumed already schema-initialized by its
/// constructor). Panics on a failed assertion; returns `Err` on a backend error.
pub fn run(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    events(store, &pid)?;
    projects_keys_limits(store, &pid)?;
    scores(store, &pid)?;
    traces(store)?;
    parity_gap_methods(store)?;
    prices(store)?;
    benchmarks(store, &pid)?;
    datasets(store, &pid)?;
    rubrics(store, &pid)?;
    jobs(store)?;
    admission(store)?;
    admission_batch(store)?;
    admission_race(store)?;
    revenue(store)?;
    relay(store, &pid)?;
    Ok(())
}

fn sample_event(pid: &str, model: &str, inp: u64, out: u64, cost: f64) -> LlmEvent {
    LlmEvent {
        id: new_id(),
        project_id: pid.into(),
        trace_id: Some("trace".into()),
        span_id: None,
        parent_span_id: None,
        ts: Utc::now(),
        received_at: Utc::now(),
        provider: Provider::Anthropic,
        model: model.into(),
        name: None,
        operation: Operation::Chat,
        usage: TokenUsage {
            input: inp,
            output: out,
            cached_input: None,
            reasoning: None,
        },
        cost_usd: Some(cost),
        latency_ms: Some(42),
        status: Status::Success,
        error: None,
        input: Some(json!({ "q": "hi" })),
        output: Some(json!({ "a": "yo" })),
        tags: vec!["conf".into()],
        source: Some("conformance".into()),
        metadata: json!({ "k": "v" }),
    }
}

/// A monitored event attributed to a billing `customer` (the linkage `cost_by_dimension` groups on,
/// read from `metadata.customer_id`).
fn tagged_event(pid: &str, customer: &str, cost: f64) -> LlmEvent {
    let mut ev = sample_event(pid, "claude-haiku-4-5", 10, 5, cost);
    ev.metadata = json!({ "customer_id": customer });
    ev
}

fn events(store: &dyn Store, pid: &str) -> Result<()> {
    store.insert_event(&sample_event(pid, "claude-haiku-4-5", 100, 50, 0.001))?;
    store.insert_event(&sample_event(pid, "claude-haiku-4-5", 200, 80, 0.002))?;

    let listed = store.list_events(Some(pid), 10)?;
    assert_eq!(listed.len(), 2, "list_events scoped to project");
    assert_eq!(listed[0].project_id, pid);
    assert_eq!(listed[0].tags, vec!["conf".to_string()]);
    assert_eq!(
        listed[0].metadata,
        json!({ "k": "v" }),
        "metadata round-trip"
    );
    assert!(
        listed[0].input.is_some() && listed[0].output.is_some(),
        "payload round-trip"
    );

    let one = store.get_event(&listed[0].id)?.expect("get_event Some");
    assert_eq!(one.id, listed[0].id);
    assert!(
        store.get_event(&new_id())?.is_none(),
        "get_event None for unknown id"
    );

    // Re-inserting an existing id must be a typed Conflict on every backend — not an opaque
    // error (Postgres pre-23505-mapping) and never a silent overwrite (Firestore pre-precondition
    // upsert). The API's 409 / idempotency contract rides this variant.
    match store.insert_event(&one) {
        Err(crate::StoreError::Conflict(_)) => {}
        other => panic!("duplicate insert_event must be Err(Conflict), got {other:?}"),
    }
    assert_eq!(
        store.list_events(Some(pid), 10)?.len(),
        2,
        "duplicate insert persisted nothing"
    );

    let costs = store.cost_summary(Some(pid))?;
    assert_eq!(costs.len(), 1, "one (provider,model) group");
    assert_eq!(costs[0].calls, 2);
    assert_eq!(costs[0].input_tokens, 300);
    assert_eq!(costs[0].output_tokens, 130);
    assert!((costs[0].cost_usd - 0.003).abs() < 1e-9, "cost sum");

    let since = Utc::now() - chrono::Duration::hours(1);
    let u = store.usage_since(pid, since)?;
    assert_eq!(u.calls, 2);
    assert_eq!(u.tokens, 430);
    assert!((u.cost_usd - 0.003).abs() < 1e-9, "usage cost");

    // Per-key attribution. `metadata.api_key_id` is server-stamped at ingest and is the dimension a
    // per-key budget scopes to, so a backend that can't read it turns "cap the staging key" into a
    // cap on nothing (or, if it fell back to project-wide, a cap on everything). Both readings —
    // one key's total and the grouped breakdown — are part of the contract.
    let mut keyed = sample_event(pid, "claude-haiku-4-5", 7, 3, 0.004);
    keyed.metadata = json!({ "api_key_id": "conf-key-1" });
    store.insert_event(&keyed)?;

    let k = store.usage_since_scoped(pid, since, &LimitScope::ApiKey("conf-key-1".into()))?;
    assert_eq!(k.calls, 1, "only the keyed event counts toward that key");
    assert!((k.cost_usd - 0.004).abs() < 1e-9);
    let none =
        store.usage_since_scoped(pid, since, &LimitScope::ApiKey("conf-key-absent".into()))?;
    assert_eq!(
        none.calls, 0,
        "an unknown key has no usage (never the project-wide total)"
    );

    let by_key = store.usage_by_scope(pid, since, "api_key")?;
    let keyed_row = by_key
        .iter()
        .find(|r| r.value.as_deref() == Some("conf-key-1"))
        .expect("the keyed row is present in the breakdown");
    assert_eq!(keyed_row.usage.calls, 1);
    let unattributed = by_key
        .iter()
        .find(|r| r.value.is_none())
        .expect("events with no key fold into one unattributed bucket, they are not dropped");
    assert_eq!(unattributed.usage.calls, 2);
    assert_eq!(
        by_key.iter().map(|r| r.usage.calls).sum::<i64>(),
        3,
        "the breakdown's parts sum to the window's total"
    );
    assert!(
        store.usage_by_scope(pid, since, "not-a-dimension").is_err(),
        "an unknown dimension is an error, not an empty (authoritative-looking) breakdown"
    );
    Ok(())
}

fn projects_keys_limits(store: &dyn Store, pid: &str) -> Result<()> {
    let proj = Project {
        id: pid.into(),
        name: "conf".into(),
        enabled: true,
        redaction: Redaction::None,
        // Non-default on purpose: pins that the consent flag round-trips on every backend (a backend
        // that drops it silently opts a project out of — or worse, into — collective contribution).
        collective_opt_in: true,
        created_at: Utc::now(),
    };
    store.create_project(&proj)?;
    let got = store.get_project(pid)?.expect("get_project Some");
    assert!(got.collective_opt_in, "collective_opt_in round-trips");
    assert!(store.get_project(&new_id())?.is_none(), "get_project None");
    assert!(
        store.list_projects()?.iter().any(|p| p.id == pid),
        "list_projects contains ours"
    );

    let prefix: String = new_id().chars().take(8).collect();
    let key = ApiKey {
        id: new_id(),
        project_id: pid.into(),
        name: "default".into(),
        prefix: prefix.clone(),
        key_hash: "salt:hash".into(),
        created_at: Utc::now(),
        last_used_at: None,
        revoked: false,
    };
    store.create_api_key(&key)?;
    let found = store
        .find_api_key_by_prefix(&prefix)?
        .expect("find_api_key_by_prefix Some");
    assert_eq!(found.project_id, pid);
    assert!(
        store.find_api_key_by_prefix("zzzzzzzz")?.is_none(),
        "unknown prefix None"
    );
    store.touch_api_key(&key.id, Utc::now())?;

    // Key lifecycle: the project's keys are listable (with the last-use we just stamped), and a key
    // can be revoked — the two fields that were write-only / enforced-but-unsettable before this wave.
    let keys = store.list_api_keys(pid)?;
    assert!(
        keys.iter().any(|k| k.id == key.id),
        "list_api_keys contains our key"
    );
    assert!(
        keys.iter()
            .find(|k| k.id == key.id)
            .unwrap()
            .last_used_at
            .is_some(),
        "last_used_at is readable back"
    );
    assert!(
        store.set_api_key_revoked(&key.id, true)?,
        "revoke reports a row changed"
    );
    assert!(
        store
            .find_api_key_by_prefix(&prefix)?
            .expect("still present")
            .revoked,
        "revoked persisted"
    );
    assert!(
        !store.set_api_key_revoked(&new_id(), true)?,
        "revoking an unknown id returns false"
    );

    let rule = LimitRule {
        id: new_id(),
        project_id: pid.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: 0.0015,
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: None,
    };
    store.create_limit_rule(&rule)?;
    let enabled = store.list_limit_rules(pid, true)?;
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].metric, LimitMetric::CostUsd);
    let u = store.usage_since(pid, Utc::now() - chrono::Duration::hours(1))?;
    assert!(
        rule.evaluate(u.cost_usd).breached,
        "0.003 cost should breach 0.0015 threshold"
    );

    // Scoped-rule lifecycle round-trip: `warn_at` + `scope` must persist faithfully — a backend
    // that drops them turns "cap gpt-4o at $X" into an unscoped project-wide cap (a semantic
    // inversion, not an absence) — and get/update/delete must work wherever create does.
    let scoped = LimitRule {
        id: new_id(),
        project_id: pid.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Day,
        threshold: 50.0,
        action: LimitAction::Throttle,
        enabled: true,
        warn_at: Some(0.8),
        scope: Some(LimitScope::Model("conf-scoped-model".into())),
    };
    store.create_limit_rule(&scoped)?;
    let got = store
        .get_limit_rule(&scoped.id)?
        .expect("get_limit_rule finds the rule");
    assert_eq!(got.warn_at, Some(0.8), "warn_at round-trips");
    assert_eq!(
        got.scope,
        Some(LimitScope::Model("conf-scoped-model".into())),
        "scope round-trips (dropping it silently widens a scoped cap to the whole project)"
    );
    let mut updated = got.clone();
    updated.threshold = 75.0;
    updated.scope = Some(LimitScope::Provider("conf-prov".into()));
    assert!(store.update_limit_rule(&updated)?, "update matches the row");
    let after = store
        .get_limit_rule(&scoped.id)?
        .expect("rule still present after update");
    assert!(
        (after.threshold - 75.0).abs() < 1e-9,
        "threshold update persists"
    );
    assert_eq!(
        after.scope,
        Some(LimitScope::Provider("conf-prov".into())),
        "scope update persists"
    );
    // The key/customer dimensions must survive the same round-trip — a backend that dropped an
    // unknown `scope_kind` would silently promote a $5 staging cap to a project-wide one.
    for s in [
        LimitScope::ApiKey("conf-key-1".into()),
        LimitScope::Customer("conf-cus".into()),
    ] {
        let mut r = after.clone();
        r.scope = Some(s.clone());
        assert!(store.update_limit_rule(&r)?);
        assert_eq!(
            store.get_limit_rule(&scoped.id)?.and_then(|g| g.scope),
            Some(s.clone()),
            "{} scope round-trips",
            s.kind_str()
        );
    }
    assert!(
        store.delete_limit_rule(&scoped.id)?,
        "delete removes the rule"
    );
    assert!(
        store.get_limit_rule(&scoped.id)?.is_none(),
        "deleted rule is gone"
    );
    assert!(
        !store.delete_limit_rule(&new_id())?,
        "deleting an unknown id returns false"
    );
    Ok(())
}

fn scores(store: &dyn Store, pid: &str) -> Result<()> {
    let s = Score {
        id: new_id(),
        project_id: pid.into(),
        event_id: None,
        rubric: "correctness".into(),
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
    let listed = store.list_scores(Some(pid), 10)?;
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

    let scored_set = store.scored_event_ids(&[scored_ev.id.clone(), unscored_ev.id.clone()])?;
    assert_eq!(
        scored_set,
        vec![scored_ev.id.clone()],
        "only the scored event id comes back"
    );
    assert!(
        store.scored_event_ids(&[])?.is_empty(),
        "empty input -> empty output"
    );

    let unscored = store.list_unscored_events(Some(pid), 50)?;
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
        }],
        agreement: Some(0.75),
        samples_requested: Some(3),
        samples_parsed: Some(2),
        parse_failures: Some(1),
        injection_suspected: Some(false),
        determinism: Some("exact".into()),
        ..Default::default()
    };
    let case = |run: &str, idx: Option<u32>, value: f64| Score {
        id: new_id(),
        project_id: pid.into(),
        event_id: None,
        rubric: "bench:conformance".into(),
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

    let cases = store.list_run_scores(&run_id, Some(pid), 100)?;
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
    // Authorization scope is applied in the query, not by the caller.
    assert!(
        store
            .list_run_scores(&run_id, Some(&new_id()), 100)?
            .is_empty(),
        "another project's key sees none of this run's cases"
    );
    assert!(
        store.list_run_scores(&new_id(), None, 100)?.is_empty(),
        "unknown run -> no cases"
    );
    assert_eq!(
        store.list_run_scores(&run_id, None, 2)?.len(),
        2,
        "limit is honored"
    );
    Ok(())
}

/// Exercises the trait's default-bearing query methods — `list_events_filtered`,
/// `cost_summary_windowed`, `usage_since_scoped`, `usecase_costs` — which the SQLite backend overrides
/// but Postgres/Firestore currently inherit. The inherited defaults return *plausible-but-wrong* data
/// (an unfiltered list, all-time cost, project-wide usage, an empty rollup), so before this section
/// the suite passed a backend that silently answered these wrong. It pins the correct behavior against
/// SQLite and will now fail any backend that hasn't ported these queries — the drift signal the
/// systemic parity gap was missing. Scoped to a fresh project so the window/scope math is deterministic.
fn parity_gap_methods(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let now = Utc::now();
    let mk = |model: &str, name: &str, cost: f64, ts: chrono::DateTime<Utc>| {
        let mut e = sample_event(&pid, model, 10, 5, cost);
        e.name = Some(name.into());
        e.ts = ts;
        e
    };
    store.insert_event(&mk("m-a", "gen", 1.0, now))?;
    store.insert_event(&mk("m-b", "summarize", 2.0, now))?;
    store.insert_event(&mk("m-a", "gen", 4.0, now - chrono::Duration::hours(48)))?;

    // list_events_filtered: a model filter must actually filter (the default returns ALL events).
    let filter = EventFilter {
        model: Some("m-b".into()),
        ..Default::default()
    };
    let page = store.list_events_filtered(Some(&pid), &filter, 50)?;
    assert_eq!(
        page.events.len(),
        1,
        "model filter returns only m-b (default would return all 3)"
    );
    assert_eq!(page.events[0].model, "m-b");

    // cost_summary_windowed: a 1h window excludes the 48h-old event (the default returns all-time).
    let since = now - chrono::Duration::hours(1);
    let windowed = store.cost_summary_windowed(Some(&pid), Some(since), None)?;
    let total: f64 = windowed.iter().map(|c| c.cost_usd).sum();
    assert!(
        (total - 3.0).abs() < 1e-9,
        "windowed cost = a+b = 3.0, not all-time 7.0 (got {total})"
    );

    // usage_since_scoped: scoping to model m-b sees only b (the default falls back to project-wide).
    let scoped = store.usage_since_scoped(&pid, since, &LimitScope::Model("m-b".into()))?;
    assert_eq!(
        scoped.calls, 1,
        "scoped usage counts only m-b (default would count both)"
    );
    assert!((scoped.cost_usd - 2.0).abs() < 1e-9);

    // usecase_costs: groups by (name, provider, model) within the window (the default returns empty).
    let uc = store.usecase_costs(Some(&pid), Some(since))?;
    let summarize = uc
        .iter()
        .find(|r| r.name.as_deref() == Some("summarize"))
        .expect("summarize use-case group present (default returns an empty rollup)");
    assert_eq!(summarize.calls, 1);
    assert!((summarize.cost_usd - 2.0).abs() < 1e-9);

    // Keyset paging: 3 events, page size 2 → one continuation page, then exhaustion. No event may
    // be duplicated or skipped across the page boundary (the default mints no cursor at all).
    let page1 = store.list_events_filtered(Some(&pid), &EventFilter::default(), 2)?;
    assert_eq!(page1.events.len(), 2, "first page fills to the limit");
    let cursor = page1
        .next_cursor
        .clone()
        .expect("more rows exist -> next_cursor is minted");
    let page2 = store.list_events_filtered(
        Some(&pid),
        &EventFilter {
            cursor: Some(cursor),
            ..Default::default()
        },
        2,
    )?;
    assert_eq!(
        page2.events.len(),
        1,
        "second page holds the remaining event"
    );
    assert!(
        page2.next_cursor.is_none(),
        "exhausted -> no further cursor"
    );
    let mut ids: Vec<&str> = page1
        .events
        .iter()
        .chain(page2.events.iter())
        .map(|e| e.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        3,
        "no duplicate or skipped events across the page boundary"
    );

    // Predicates AND-combine: model + name + window jointly isolate the single recent m-a event.
    let filter = EventFilter {
        model: Some("m-a".into()),
        name: Some("gen".into()),
        since: Some(since),
        ..Default::default()
    };
    let anded = store.list_events_filtered(Some(&pid), &filter, 50)?;
    assert_eq!(
        anded.events.len(),
        1,
        "model+name+since AND together (not OR / not ignored)"
    );
    assert_eq!(anded.events[0].model, "m-a");
    Ok(())
}

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
fn traces(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let other = new_id();
    let tid = format!("t-{}", new_id());

    if !store.serves_traces() {
        // An honest refusal is a documented limitation (see the Firestore backend); a silent empty
        // page is not. Every entry point must say so.
        let refused = |what: &str, r: Result<()>| match r {
            Err(StoreError::Unsupported(_)) => {}
            got => panic!(
                "{what} must refuse with Unsupported on a backend that does not serve traces, \
                 got {got:?}"
            ),
        };
        refused("list_traces", store.list_traces(Some(&pid), 10).map(|_| ()));
        refused(
            "list_traces_filtered",
            store
                .list_traces_filtered(Some(&pid), &TraceFilter::default(), 10)
                .map(|_| ()),
        );
        refused(
            "list_trace_events",
            store.list_trace_events(Some(&pid), &tid, 10).map(|_| ()),
        );
        refused(
            "list_trace_scores",
            store.list_trace_scores(Some(&pid), &tid).map(|_| ()),
        );
        refused(
            "get_trace",
            store.get_trace(Some(&pid), &tid, 10).map(|_| ()),
        );
        return Ok(());
    }

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

fn prices(store: &dyn Store) -> Result<()> {
    let model = format!("conf-{}", new_id());
    let mut row = ModelPriceRow {
        provider: "conformance".into(),
        model: model.clone(),
        input_per_mtok: 1.0,
        output_per_mtok: 2.0,
        cached_input_per_mtok: Some(0.1),
        effective_date: Utc::now(),
        source_url: None,
    };
    store.upsert_price(&row)?;
    let found = store
        .list_prices()?
        .into_iter()
        .find(|p| p.provider == "conformance" && p.model == model)
        .expect("upserted price present");
    assert!((found.input_per_mtok - 1.0).abs() < 1e-9);

    // Conflict path: a second upsert on the same (provider, model) updates in place.
    row.output_per_mtok = 9.0;
    store.upsert_price(&row)?;
    let updated = store
        .list_prices()?
        .into_iter()
        .find(|p| p.model == model)
        .expect("price still present");
    assert!(
        (updated.output_per_mtok - 9.0).abs() < 1e-9,
        "upsert ON CONFLICT updates"
    );
    Ok(())
}

fn benchmarks(store: &dyn Store, pid: &str) -> Result<()> {
    let target = json!([{ "provider": "anthropic", "model": "haiku" }]);
    let b = Benchmark {
        id: new_id(),
        project_id: pid.into(),
        name: "bench".into(),
        rubric: "is it right".into(),
        judge_model: "haiku".into(),
        target: target.clone(),
        dataset_ref: None,
        rubric_id: None,
        dataset: vec![BenchmarkCase {
            input: "2+2".into(),
            expected: Some("4".into()),
            output: Some("4".into()),
        }],
        baseline_score: Some(0.8),
        created_at: Utc::now(),
    };
    store.create_benchmark(&b)?;
    let got = store.get_benchmark(&b.id)?.expect("get_benchmark Some");
    assert_eq!(got.name, "bench");
    assert_eq!(got.dataset.len(), 1);
    assert_eq!(got.target, target, "benchmark target round-trip");
    assert!(store.list_benchmarks(pid)?.iter().any(|x| x.id == b.id));

    let run = BenchmarkRun {
        id: new_id(),
        benchmark_id: b.id.clone(),
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        n_cases: 1,
        mean_score: Some(1.0),
        pass_rate: Some(1.0),
        cost_usd: 0.005,
        status: "passed".into(),
        p50_latency_ms: Some(100),
        p95_latency_ms: Some(200),
        total_tokens: Some(123),
        report: json!({ "note": "ok" }),
    };
    store.create_benchmark_run(&run)?;
    let runs = store.list_benchmark_runs(&b.id)?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].n_cases, 1);
    assert_eq!(runs[0].total_tokens, Some(123));
    assert_eq!(
        runs[0].report,
        json!({ "note": "ok" }),
        "run report round-trip"
    );
    Ok(())
}

fn datasets(store: &dyn Store, pid: &str) -> Result<()> {
    let d = Dataset {
        id: new_id(),
        project_id: pid.into(),
        name: "ds".into(),
        version: 1,
        frozen: false,
        source: Some("conf".into()),
        created_at: Utc::now(),
    };
    store.create_dataset(&d)?;
    assert!(store.get_dataset(&d.id)?.is_some());
    assert!(store.list_datasets(pid)?.iter().any(|x| x.id == d.id));

    let item = DatasetItem {
        id: new_id(),
        dataset_id: d.id.clone(),
        input: "2+2".into(),
        output: None,
        expected: Some("4".into()),
        context: None,
        tags: vec!["t".into()],
        source_event_id: None,
        anonymization: json!({ "method": "regex", "redactions": 0 }),
    };
    store.create_dataset_item(&item)?;
    let items = store.list_dataset_items(&d.id)?;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].expected, Some("4".to_string()));
    assert_eq!(
        items[0].anonymization,
        json!({ "method": "regex", "redactions": 0 })
    );

    store.set_dataset_frozen(&d.id, true)?;
    assert!(
        store.get_dataset(&d.id)?.expect("dataset").frozen,
        "frozen after set"
    );
    Ok(())
}

fn rubrics(store: &dyn Store, pid: &str) -> Result<()> {
    let r = Rubric {
        id: new_id(),
        project_id: pid.into(),
        name: "rub".into(),
        dimensions: vec![
            RubricDimension {
                key: "correct".into(),
                description: "is it right".into(),
                weight: 1.0,
                anchors: vec!["1.0 = yes".into()],
                floor: Some(0.5),
                kind: DimensionKind::Llm,
                check: DimensionCheck::default(),
            },
            // A deterministic dimension: its kind + config must survive the round-trip on every
            // backend, or a mixed rubric would silently degrade to all-LLM after a reload.
            RubricDimension {
                key: "answer".into(),
                description: "exact answer".into(),
                weight: 2.0,
                anchors: vec![],
                floor: Some(1.0),
                kind: DimensionKind::Numeric,
                check: DimensionCheck {
                    expect: Some("42".into()),
                    tolerance: Some(0.1),
                    ..Default::default()
                },
            },
        ],
        threshold: 0.7,
        created_at: Utc::now(),
    };
    store.create_rubric(&r)?;
    let got = store.get_rubric(&r.id)?.expect("get_rubric Some");
    assert_eq!(got.dimensions.len(), 2);
    assert_eq!(got.dimensions[0].key, "correct");
    assert_eq!(got.dimensions[0].floor, Some(0.5));
    assert_eq!(got.dimensions[0].kind, DimensionKind::Llm);
    assert_eq!(got.dimensions[1].kind, DimensionKind::Numeric);
    assert_eq!(got.dimensions[1].check.expect.as_deref(), Some("42"));
    assert_eq!(got.dimensions[1].check.tolerance, Some(0.1));
    assert!(store.list_rubrics(pid)?.iter().any(|x| x.id == r.id));
    Ok(())
}

fn admission(store: &dyn Store) -> Result<()> {
    let pid = new_id();

    // No rules configured: every event is admitted and recorded.
    let first = store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(first.admitted, "no rules -> admitted");
    assert!(first.statuses.is_empty(), "no rules -> no statuses");

    // An Alert rule breaches but never blocks: the event is still recorded.
    let alert = LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: 1.0,
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: None,
    };
    store.create_limit_rule(&alert)?;
    let alerted =
        store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(alerted.admitted, "Alert action never blocks ingest");
    assert!(
        alerted.statuses.iter().any(|s| s.breached),
        "Alert rule reports the breach"
    );

    // A Block rule on cost: usage is 2.0 so far; threshold 2.5. The next $1.0 event would push
    // usage-with-this-event to 3.0 >= 2.5, so it is rejected and not recorded.
    let block = LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: 2.5,
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: None,
    };
    store.create_limit_rule(&block)?;
    let blocked =
        store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(!blocked.admitted, "Block rule rejects an over-cap event");
    assert!(
        blocked.statuses.iter().any(|s| s.rejects_ingest()),
        "rejection carries a breached enforcing status"
    );

    // The rejected event was never recorded: usage stays at the two admitted events.
    let u = store.usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?;
    assert_eq!(u.calls, 2, "only the two admitted events are recorded");
    assert!(
        (u.cost_usd - 2.0).abs() < 1e-9,
        "rejected event's cost not counted"
    );
    Ok(())
}

/// Batch admission ([`Store::insert_events_checked`]): one result per item, in order; items already
/// accepted *earlier in the same batch* count toward the cap (so a caller can't bypass a limit by
/// packing events into one request); and a per-item store error lands in that item's slot instead of
/// poisoning the rest — the property a single-transaction port must not lose (on Postgres an
/// un-savepointed error aborts the whole transaction).
fn admission_batch(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    store.create_limit_rule(&LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: 3.0,
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: None,
    })?;
    let batch: Vec<LlmEvent> = (0..5)
        .map(|_| sample_event(&pid, "claude-haiku-4-5", 1, 1, 0.0))
        .collect();
    let results = store.insert_events_checked(&batch);
    assert_eq!(results.len(), 5, "one result per batch item, in order");
    let mut admitted = 0;
    for r in results {
        if r?.admitted {
            admitted += 1;
        }
    }
    assert_eq!(
        admitted, 2,
        "in-batch accepted items count toward the cap of 3"
    );
    assert_eq!(
        store
            .usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?
            .calls,
        2,
        "only the admitted items were persisted"
    );

    // Per-item failure isolation, on an uncapped project: a duplicate id in the middle must not cost
    // the items around it.
    let pid2 = new_id();
    let first = sample_event(&pid2, "claude-haiku-4-5", 1, 1, 0.0);
    let third = sample_event(&pid2, "claude-haiku-4-5", 1, 1, 0.0);
    let mixed = store.insert_events_checked(&[first.clone(), first.clone(), third]);
    assert!(
        matches!(mixed[0], Ok(ref a) if a.admitted),
        "first item admitted"
    );
    assert!(
        matches!(mixed[1], Err(crate::StoreError::Conflict(_))),
        "duplicate id is a typed per-item Conflict, got {:?}",
        mixed[1]
    );
    assert!(
        matches!(mixed[2], Ok(ref a) if a.admitted),
        "an item after a failed one still lands (the batch is not poisoned), got {:?}",
        mixed[2]
    );
    assert_eq!(
        store
            .usage_since(&pid2, Utc::now() - chrono::Duration::hours(1))?
            .calls,
        2,
        "the two distinct events are stored; the duplicate added nothing"
    );
    Ok(())
}

/// What a concurrent burst did to one cap: how many events the backend admitted, how many it
/// actually persisted, and the cap they were racing.
#[derive(Debug, Clone, Copy)]
pub struct RaceOutcome {
    /// The `calls` threshold the burst raced. An atomic backend admits at most `cap - 1` events (the
    /// event that would reach the threshold is the one rejected).
    pub cap: i64,
    pub admitted: i64,
    /// Events readable back from the store afterwards — must equal `admitted` (a rejected event is
    /// never recorded).
    pub stored: i64,
}

/// Fire `RACERS` simultaneous admissions at one fresh project guarded by a `Block` cap and report
/// what got through. Exposed (rather than inlined into [`run`]) so a caller can point it at a
/// *specific* admission path — the suite points it at [`Store::insert_event_checked`], and the
/// crate's own test points it at the trait's non-atomic default to prove this probe actually bites.
///
/// The barrier is the whole point: without it the calls trickle in and even a check-then-act
/// implementation looks correct, which is exactly how the cloud backends' advisory caps survived
/// review.
pub fn admission_race_probe(
    store: &dyn Store,
    admit: &(dyn Fn(&dyn Store, &LlmEvent) -> Result<Admission> + Sync),
) -> Result<RaceOutcome> {
    const RACERS: usize = 8;
    const CAP: i64 = 4;

    let pid = new_id();
    store.create_limit_rule(&LimitRule {
        id: new_id(),
        project_id: pid.clone(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: CAP as f64,
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: None,
    })?;

    let evs: Vec<LlmEvent> = (0..RACERS)
        .map(|_| sample_event(&pid, "claude-haiku-4-5", 1, 1, 0.0))
        .collect();
    let barrier = std::sync::Barrier::new(RACERS);
    let results: Vec<Result<Admission>> = std::thread::scope(|s| {
        let handles: Vec<_> = evs
            .iter()
            .map(|ev| {
                s.spawn(|| {
                    barrier.wait();
                    admit(store, ev)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("admission thread panicked"))
            .collect()
    });

    let mut admitted = 0;
    for r in results {
        if r?.admitted {
            admitted += 1;
        }
    }
    let stored = store
        .usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?
        .calls;
    Ok(RaceOutcome {
        cap: CAP,
        admitted,
        stored,
    })
}

/// The cap must hold under a **simultaneous** burst, not merely under serial traffic — the property
/// `admission` above cannot see. A backend whose check-then-insert isn't one critical section lets
/// every racer read the same pre-burst usage and admit, so a cap of 4 quietly passes 8 events.
///
/// Backends that declare [`Store::admission_is_atomic`] `= false` are *reported*, not failed: an
/// honest advisory cap is a documented limitation (see the Firestore backend), while a backend
/// claiming atomicity and leaking is a correctness bug.
fn admission_race(store: &dyn Store) -> Result<()> {
    let out = admission_race_probe(store, &|s, e| s.insert_event_checked(e))?;
    assert_eq!(
        out.stored, out.admitted,
        "every admitted event is recorded, and only those"
    );
    if store.admission_is_atomic() {
        assert!(
            out.admitted < out.cap,
            "atomic admission must keep a concurrent burst under the cap: {out:?}"
        );
    } else if out.admitted >= out.cap {
        eprintln!(
            "admission is advisory on this backend (admission_is_atomic() == false): a burst \
             admitted {} events against a cap of {}",
            out.admitted, out.cap
        );
    }
    Ok(())
}

/// Revenue + margin (Phase 1 profit tracking). This is the check that catches a backend silently
/// inheriting the trait's no-op revenue defaults (e.g. a backend with no `revenue.rs`): a no-op
/// `insert_revenue_event` errors here, and a no-op `list`/`cost_by_dimension` returns empty and trips
/// the round-trip assertions. Scoped to a fresh project so `cost_by_dimension` (which reads event
/// metadata over a window) sees only the traffic this check inserts.
///
/// It also pins the **idempotent-upsert** invariant: a redelivered webhook — a fresh record sharing
/// the deterministic `stripe:<external_id>` id `normalize_invoice` mints — must upsert onto the
/// existing row, so revenue and every margin number derived from it is recognized exactly once. A
/// backend that keyed off a surrogate row id instead would double-count, and this check fails it.
fn revenue(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    // Monitored traffic for two customers: `heavy` is the money-loser.
    store.insert_event(&tagged_event(&pid, "acme", 0.50))?;
    store.insert_event(&tagged_event(&pid, "acme", 0.37))?;
    store.insert_event(&tagged_event(&pid, "heavy", 142.5))?;

    let now = Utc::now();
    // Mirror `billing::normalize_invoice`: a synced record carries a *deterministic* id derived from
    // its external (provider) id — `stripe:<external_id>` — which is the key a redelivered webhook
    // collapses onto. Building ids this way lets the replay below exercise the real idempotency path
    // rather than the trivial re-insert-the-same-struct case.
    let mk_rev = |customer: &str, amount: f64| {
        let external_id = format!("inv-{customer}");
        RevenueEvent {
            id: format!("stripe:{external_id}"),
            project_id: pid.clone(),
            source: "stripe".into(),
            external_id: Some(external_id),
            customer_id: Some(customer.into()),
            product_id: None,
            amount_usd: amount,
            currency: "USD".into(),
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts: now,
        }
    };
    // The batch path (atomic on backends that override it, a per-record loop otherwise).
    store.insert_revenue_events(&[mk_rev("acme", 20.0), mk_rev("heavy", 99.0)])?;

    let since = now - chrono::Duration::hours(1);
    let until = now + chrono::Duration::hours(1);

    let listed = store.list_revenue_events(Some(&pid), since, until)?;
    assert_eq!(
        listed.len(),
        2,
        "both point-in-time revenue records recognized in window"
    );
    assert!(
        listed.iter().all(|r| r.project_id == pid),
        "list scoped to project"
    );
    let got_acme = listed
        .iter()
        .find(|r| r.customer_id.as_deref() == Some("acme"))
        .expect("acme revenue present");
    assert!(
        (got_acme.amount_usd - 20.0).abs() < 1e-9,
        "amount round-trip"
    );
    assert_eq!(
        got_acme.external_id.as_deref(),
        Some("inv-acme"),
        "external_id round-trip"
    );
    assert_eq!(got_acme.kind, RevenueKind::OneTime, "kind round-trip");

    // A replayed Stripe webhook: `normalize_invoice` runs again on the redelivery and yields a *fresh*
    // record carrying the same deterministic id (`stripe:<external_id>`). The upsert must collapse it
    // onto the existing row — a second physical row here would silently double every downstream margin
    // number, the exact corruption profit tracking exists to prevent.
    store.insert_revenue_event(&mk_rev("acme", 20.0))?;
    let after = store.list_revenue_events(Some(&pid), since, until)?;
    assert_eq!(
        after.len(),
        2,
        "redelivered webhook upserts; total revenue row count unchanged"
    );
    assert_eq!(
        after
            .iter()
            .filter(|r| r.external_id.as_deref() == Some("inv-acme"))
            .count(),
        1,
        "acme stays a single row after replay — no double-count",
    );

    // Cost grouped by the billing dimension, read from event metadata.
    let costs = store.cost_by_dimension(Some(&pid), "customer", since, until)?;
    let acme_cost = costs
        .iter()
        .find(|c| c.key.as_deref() == Some("acme"))
        .expect("acme cost group");
    assert_eq!(acme_cost.calls, 2);
    assert!(
        (acme_cost.cost_usd - 0.87).abs() < 1e-9,
        "acme cost summed across its events"
    );
    let heavy_cost = costs
        .iter()
        .find(|c| c.key.as_deref() == Some("heavy"))
        .expect("heavy cost group");
    assert_eq!(heavy_cost.calls, 1);
    assert!((heavy_cost.cost_usd - 142.5).abs() < 1e-9);

    // End-to-end over the post-replay set: the unprofitable customer surfaces first (margin ascending),
    // and acme's $20 is recognized exactly once despite the redelivery.
    let rows = compute_margin(&after, &costs, MarginDimension::Customer, since, until);
    assert_eq!(rows[0].key, "heavy", "money-loser sorts first");
    assert!((rows[0].gross_margin_usd - (99.0 - 142.5)).abs() < 1e-6);
    let acme_row = rows
        .iter()
        .find(|r| r.key == "acme")
        .expect("acme margin row");
    assert!(
        (acme_row.revenue_usd - 20.0).abs() < 1e-9,
        "revenue recognized once, not doubled"
    );
    assert!(
        (acme_row.gross_margin_usd - 19.13).abs() < 1e-9,
        "revenue − attributed cost"
    );
    Ok(())
}

fn new_job() -> Job {
    let now = Utc::now();
    Job {
        id: new_id(),
        job_type: "conf".into(),
        payload: json!({ "k": "v" }),
        status: "queued".into(),
        attempts: 0,
        max_attempts: 3,
        failures: 0,
        stale_reclaims: 0,
        progress: None,
        error: None,
        result: Value::Null,
        claimed_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn jobs(store: &dyn Store) -> Result<()> {
    let now = Utc::now();
    let j = new_job();
    store.create_job(&j)?;
    assert_eq!(
        store.get_job(&j.id)?.expect("get_job Some").status,
        "queued"
    );

    // Claim is global (oldest queued/stale first), so on a shared DB it may return another job —
    // assert only that a job was claimed and flipped to running with a bumped attempt count.
    let claimed = store.claim_job(now)?.expect("claim_job returns a job");
    assert_eq!(claimed.status, "running");
    assert!(claimed.attempts >= 1, "claim bumps attempts");

    // Our specific job's lifecycle by id (independent of which job claim() returned).
    store.update_job_progress(&j.id, "50%")?;
    store.finish_job(&j.id, "done", &json!({ "ok": true }), None)?;
    let done = store.get_job(&j.id)?.expect("get_job after finish");
    assert_eq!(done.status, "done");
    assert_eq!(done.result, json!({ "ok": true }), "job result round-trip");
    assert!(store
        .list_jobs(Some("done"), 100)?
        .iter()
        .any(|x| x.id == j.id));
    job_cancellation(store)?;
    job_failure_accounting(store)?;
    Ok(())
}

/// Claim until the queue is empty (bounded), returning every id claimed. Lets the cancellation
/// checks below reason about a queue whose head they control, on a store whose claim is global.
fn drain_jobs(store: &dyn Store) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for _ in 0..50 {
        match store.claim_job(Utc::now())? {
            Some(j) => ids.push(j.id),
            None => break,
        }
    }
    Ok(ids)
}

/// Cancellation, and the property that matters most about it: a cancelled run is **never restarted
/// by the stale-claim reclaim path**. A backend that can't cancel must say so (`Unsupported` → 501),
/// never quietly do nothing.
fn job_cancellation(store: &dyn Store) -> Result<()> {
    let queued = new_job();
    store.create_job(&queued)?;
    match store.cancel_job(&queued.id) {
        Err(StoreError::Unsupported(_)) => {
            eprintln!("conformance: backend does not support cancel_job (501) — skipping");
            return Ok(());
        }
        Err(e) => return Err(e),
        Ok(outcome) => assert_eq!(
            outcome,
            Some(JobCancel::Cancelled),
            "a queued job is cancelled outright — nothing ran"
        ),
    }
    assert_eq!(store.get_job(&queued.id)?.expect("get").status, "cancelled");
    // Cancelling an unknown job is None (→ 404), not a fabricated success.
    assert_eq!(store.cancel_job(&new_id())?, None);
    // Cancelling something terminal reports that nothing was stopped.
    assert!(
        matches!(
            store.cancel_job(&queued.id)?,
            Some(JobCancel::AlreadyFinished { .. })
        ),
        "re-cancelling a cancelled job must not claim to have stopped it"
    );

    // A RUNNING job: cancel marks it `cancelling`, and the reclaim path must not resurrect it even
    // though its claim is (deliberately) already stale.
    drain_jobs(store)?;
    let running = new_job();
    store.create_job(&running)?;
    let claimed = store
        .claim_job(Utc::now())?
        .expect("claim the job just enqueued");
    assert_eq!(
        claimed.id, running.id,
        "the drained queue's only job is ours"
    );
    assert_eq!(store.cancel_job(&running.id)?, Some(JobCancel::Cancelling));
    assert_eq!(
        store.get_job(&running.id)?.expect("get").status,
        "cancelling"
    );
    // `Utc::now()` as the staleness cutoff makes every claim in existence stale. The cancelled job
    // must STILL not come back — this is the race the reclaim path used to lose.
    for id in drain_jobs(store)? {
        assert_ne!(
            id, running.id,
            "a cancelled run must never be reclaimed as stale"
        );
    }
    assert_eq!(
        store.get_job(&running.id)?.expect("get").status,
        "cancelling",
        "reclaim must not have touched the cancelled job"
    );
    Ok(())
}

/// A worker that dies is not a benchmark that failed. `attempts` counts claims (crashes included),
/// `stale_reclaims` counts worker deaths, and `failures` — the retry budget — counts only runs that
/// actually reported an error.
fn job_failure_accounting(store: &dyn Store) -> Result<()> {
    drain_jobs(store)?;
    let j = new_job();
    store.create_job(&j)?;
    let first = store.claim_job(Utc::now())?.expect("claim");
    assert_eq!(first.id, j.id);
    assert_eq!(
        (first.attempts, first.failures, first.stale_reclaims),
        (1, 0, 0)
    );

    // Simulate the worker being killed: never finish, let the claim go stale, reclaim it.
    let second = store.claim_job(Utc::now())?.expect("reclaim the stale job");
    assert_eq!(second.id, j.id);
    assert_eq!(second.attempts, 2, "a claim is a claim, crash or not");
    assert_eq!(second.failures, 0, "a dead worker must not burn a retry");
    assert_eq!(
        second.stale_reclaims, 1,
        "…it is counted as a worker death instead"
    );
    assert!(
        second
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("worker lost"),
        "the stored error must say the worker died, not invent a benchmark failure: {:?}",
        second.error
    );

    // Now the benchmark itself fails: that IS a retry.
    store.finish_job(
        &j.id,
        "queued",
        &Value::Null,
        Some("benchmark failure: judge failed"),
    )?;
    let after = store.get_job(&j.id)?.expect("get");
    assert_eq!(
        after.failures, 1,
        "a reported error consumes the retry budget"
    );
    assert_eq!(
        after.stale_reclaims, 1,
        "…and is not confused with a worker death"
    );
    assert!(after
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("judge failed"));

    // A clean finish never consumes the budget.
    store.finish_job(&j.id, "done", &json!({ "ok": true }), None)?;
    assert_eq!(store.get_job(&j.id)?.expect("get").failures, 1);
    Ok(())
}

/// Relay queue (docs/RELAY.md): enqueue → lease → settle round-trips, retry/deferral accounting,
/// and the dead-letter sweep. Skips backends that don't host the relay (the trait's default
/// `create_relay_task` is a clear error). Like the job claim, lease/sweep are global (oldest-due
/// first), so on a shared DB we assert on our ids and tolerate other rows in the results.
fn relay(store: &dyn Store, pid: &str) -> Result<()> {
    fn task(pid: &str, max_attempts: u32) -> RelayTask {
        let now = Utc::now();
        RelayTask {
            id: new_id(),
            project_id: pid.into(),
            source: Some("conformance".into()),
            action_type: "conf/echo".into(),
            payload: json!({ "k": "v" }),
            status: "queued".into(),
            attempts: 0,
            max_attempts,
            retry_interval_secs: 0, // failed attempts become due again immediately
            idempotency_key: None,
            device: None,
            lease_deadline: None,
            next_attempt_at: now,
            result: Value::Null,
            error: None,
            created_at: now,
            updated_at: now,
        }
    }
    fn leased_ours(store: &dyn Store, id: &str) -> Result<Option<RelayTask>> {
        Ok(store
            .lease_relay_tasks("conf-dev", 60, 20)?
            .into_iter()
            .find(|t| t.id == id))
    }

    let mut t = task(pid, 2);
    t.idempotency_key = Some(new_id());
    match store.create_relay_task(&t) {
        // Typed capability gap (never matched on the message — error.rs forbids parsing prose):
        // a backend without the relay queue skips this section instead of failing it.
        Err(e @ crate::StoreError::Unsupported(_)) => {
            eprintln!("skipping relay conformance: {e}");
            return Ok(());
        }
        r => r?,
    }

    // Round-trip + idempotency lookup.
    let got = store.get_relay_task(&t.id)?.expect("get_relay_task Some");
    assert_eq!(got.payload, json!({ "k": "v" }), "relay payload round-trip");
    let key = t.idempotency_key.clone().unwrap();
    assert_eq!(
        store.find_relay_task_by_key(pid, &key)?.expect("by key").id,
        t.id
    );
    assert!(store
        .find_relay_task_by_key("other-project", &key)?
        .is_none());

    // Lease consumes an attempt; a failure requeues (zero interval ⇒ due again) with the error.
    let leased = leased_ours(store, &t.id)?.expect("our task leased");
    assert_eq!(leased.status, "leased");
    assert_eq!(leased.attempts, 1);
    let requeued = store
        .settle_relay_task(&t.id, &RelayOutcome::Failed("conf boom".into()))?
        .expect("settle failed");
    assert_eq!(requeued.status, "queued");
    assert_eq!(requeued.error.as_deref(), Some("conf boom"));

    // A deferral hands the consumed attempt back.
    assert_eq!(leased_ours(store, &t.id)?.expect("re-leased").attempts, 2);
    let deferred = store
        .settle_relay_task(
            &t.id,
            &RelayOutcome::Deferred {
                retry_after_secs: Some(0),
                reason: Some("window".into()),
            },
        )?
        .expect("settle deferred");
    assert_eq!(deferred.status, "queued");
    assert_eq!(deferred.attempts, 1, "deferral hands the attempt back");

    // Success is terminal; a duplicate report returns the settled row unchanged.
    leased_ours(store, &t.id)?.expect("leased again");
    let done = store
        .settle_relay_task(&t.id, &RelayOutcome::Succeeded(json!({ "ok": true })))?
        .expect("settle succeeded");
    assert_eq!(done.status, "succeeded");
    assert_eq!(
        done.result,
        json!({ "ok": true }),
        "relay result round-trip"
    );
    let dup = store
        .settle_relay_task(&t.id, &RelayOutcome::Failed("late".into()))?
        .expect("duplicate settle");
    assert_eq!(dup.status, "succeeded", "duplicate report is a no-op");
    assert!(store
        .list_relay_tasks(Some(pid), Some("succeeded"), 100)?
        .iter()
        .any(|x| x.id == t.id));

    // Exhausted failure dead-letters…
    let doomed = task(pid, 1);
    store.create_relay_task(&doomed)?;
    leased_ours(store, &doomed.id)?.expect("doomed leased");
    let dead = store
        .settle_relay_task(&doomed.id, &RelayOutcome::Failed("final".into()))?
        .expect("settle dead");
    assert_eq!(dead.status, "dead");

    // …and so does the sweep, when a vanished device's expired lease has no attempts left.
    let vanished = task(pid, 1);
    store.create_relay_task(&vanished)?;
    let held = store.lease_relay_tasks("conf-dev", 0, 20)?; // zero-second lease: expires at once
    assert!(
        held.iter().any(|x| x.id == vanished.id),
        "vanished task leased"
    );
    let swept = store.sweep_relay_dead()?;
    let ours = swept
        .iter()
        .find(|x| x.id == vanished.id)
        .expect("sweep returns our task");
    assert_eq!(ours.status, "dead");
    assert_eq!(
        ours.error.as_deref(),
        Some("lease expired without a result")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{insert_event_checked_nonatomic, SqliteStore};

    /// A conformance check nobody has watched fail is a check nobody knows works. This pins that
    /// [`admission_race_probe`] distinguishes the two admission paths: SQLite's atomic override holds
    /// the cap, while the trait's non-atomic default — the one Postgres and Firestore inherited, over
    /// the *same* store — lets the burst through.
    #[test]
    fn race_probe_catches_the_non_atomic_admission_path() {
        let store = SqliteStore::open_in_memory().expect("in-memory store");
        for _ in 0..3 {
            let out = admission_race_probe(&store, &|s, e| s.insert_event_checked(e))
                .expect("atomic probe");
            assert!(
                out.admitted < out.cap,
                "atomic admission stays under the cap: {out:?}"
            );
        }
        // The default's usage read and insert are separate critical sections, so simultaneous racers
        // all count pre-burst usage. Sampled over a few rounds: the leak is a race, not a certainty.
        let leaked = (0..5).any(|_| {
            admission_race_probe(&store, &|s, e| insert_event_checked_nonatomic(s, e))
                .expect("non-atomic probe")
                .admitted
                >= 4
        });
        assert!(
            leaked,
            "the race probe must detect the non-atomic default over-admitting"
        );
    }
}
