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
    Job, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, LlmEvent, MarginDimension,
    ModelPriceRow, Operation, Project, Provider, Redaction, RelayOutcome, RelayTask, RevenueEvent,
    RevenueKind, Rubric, RubricDimension, Score, Status, TokenUsage,
};

use crate::{EventFilter, Result, Store};

/// Run the full conformance suite against `store` (assumed already schema-initialized by its
/// constructor). Panics on a failed assertion; returns `Err` on a backend error.
pub fn run(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    events(store, &pid)?;
    projects_keys_limits(store, &pid)?;
    scores(store, &pid)?;
    parity_gap_methods(store)?;
    prices(store)?;
    benchmarks(store, &pid)?;
    datasets(store, &pid)?;
    rubrics(store, &pid)?;
    jobs(store)?;
    admission(store)?;
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
        provider: Provider::Anthropic,
        model: model.into(),
        name: None,
        operation: Operation::Chat,
        usage: TokenUsage { input: inp, output: out, cached_input: None, reasoning: None },
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
    assert_eq!(listed[0].metadata, json!({ "k": "v" }), "metadata round-trip");
    assert!(listed[0].input.is_some() && listed[0].output.is_some(), "payload round-trip");

    let one = store.get_event(&listed[0].id)?.expect("get_event Some");
    assert_eq!(one.id, listed[0].id);
    assert!(store.get_event(&new_id())?.is_none(), "get_event None for unknown id");

    let costs = store.cost_summary(Some(pid))?;
    assert_eq!(costs.len(), 1, "one (provider,model) group");
    assert_eq!(costs[0].calls, 2);
    assert_eq!(costs[0].input_tokens, 300);
    assert_eq!(costs[0].output_tokens, 130);
    assert!((costs[0].cost_usd - 0.003).abs() < 1e-9, "cost sum");

    let u = store.usage_since(pid, Utc::now() - chrono::Duration::hours(1))?;
    assert_eq!(u.calls, 2);
    assert_eq!(u.tokens, 430);
    assert!((u.cost_usd - 0.003).abs() < 1e-9, "usage cost");
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
    assert!(store.list_projects()?.iter().any(|p| p.id == pid), "list_projects contains ours");

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
    let found = store.find_api_key_by_prefix(&prefix)?.expect("find_api_key_by_prefix Some");
    assert_eq!(found.project_id, pid);
    assert!(store.find_api_key_by_prefix("zzzzzzzz")?.is_none(), "unknown prefix None");
    store.touch_api_key(&key.id, Utc::now())?;

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
    assert!(rule.evaluate(u.cost_usd).breached, "0.003 cost should breach 0.0015 threshold");
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
    assert_eq!(scored_set, vec![scored_ev.id.clone()], "only the scored event id comes back");
    assert!(store.scored_event_ids(&[])?.is_empty(), "empty input -> empty output");

    let unscored = store.list_unscored_events(Some(pid), 50)?;
    assert!(
        unscored.iter().any(|e| e.id == unscored_ev.id),
        "unscored event is in the work list"
    );
    assert!(
        !unscored.iter().any(|e| e.id == scored_ev.id),
        "scored event is excluded from the work list",
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
    let filter = EventFilter { model: Some("m-b".into()), ..Default::default() };
    let page = store.list_events_filtered(Some(&pid), &filter, 50)?;
    assert_eq!(page.events.len(), 1, "model filter returns only m-b (default would return all 3)");
    assert_eq!(page.events[0].model, "m-b");

    // cost_summary_windowed: a 1h window excludes the 48h-old event (the default returns all-time).
    let since = now - chrono::Duration::hours(1);
    let windowed = store.cost_summary_windowed(Some(&pid), Some(since), None)?;
    let total: f64 = windowed.iter().map(|c| c.cost_usd).sum();
    assert!((total - 3.0).abs() < 1e-9, "windowed cost = a+b = 3.0, not all-time 7.0 (got {total})");

    // usage_since_scoped: scoping to model m-b sees only b (the default falls back to project-wide).
    let scoped = store.usage_since_scoped(&pid, since, &LimitScope::Model("m-b".into()))?;
    assert_eq!(scoped.calls, 1, "scoped usage counts only m-b (default would count both)");
    assert!((scoped.cost_usd - 2.0).abs() < 1e-9);

    // usecase_costs: groups by (name, provider, model) within the window (the default returns empty).
    let uc = store.usecase_costs(Some(&pid), Some(since))?;
    let summarize = uc
        .iter()
        .find(|r| r.name.as_deref() == Some("summarize"))
        .expect("summarize use-case group present (default returns an empty rollup)");
    assert_eq!(summarize.calls, 1);
    assert!((summarize.cost_usd - 2.0).abs() < 1e-9);
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
    assert!((updated.output_per_mtok - 9.0).abs() < 1e-9, "upsert ON CONFLICT updates");
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
        dataset: vec![BenchmarkCase { input: "2+2".into(), expected: Some("4".into()), output: Some("4".into()) }],
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
    assert_eq!(runs[0].report, json!({ "note": "ok" }), "run report round-trip");
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
    assert_eq!(items[0].anonymization, json!({ "method": "regex", "redactions": 0 }));

    store.set_dataset_frozen(&d.id, true)?;
    assert!(store.get_dataset(&d.id)?.expect("dataset").frozen, "frozen after set");
    Ok(())
}

fn rubrics(store: &dyn Store, pid: &str) -> Result<()> {
    let r = Rubric {
        id: new_id(),
        project_id: pid.into(),
        name: "rub".into(),
        dimensions: vec![RubricDimension {
            key: "correct".into(),
            description: "is it right".into(),
            weight: 1.0,
            anchors: vec!["1.0 = yes".into()],
            floor: Some(0.5),
        }],
        threshold: 0.7,
        created_at: Utc::now(),
    };
    store.create_rubric(&r)?;
    let got = store.get_rubric(&r.id)?.expect("get_rubric Some");
    assert_eq!(got.dimensions.len(), 1);
    assert_eq!(got.dimensions[0].key, "correct");
    assert_eq!(got.dimensions[0].floor, Some(0.5));
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
    let alerted = store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(alerted.admitted, "Alert action never blocks ingest");
    assert!(alerted.statuses.iter().any(|s| s.breached), "Alert rule reports the breach");

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
    let blocked = store.insert_event_checked(&sample_event(&pid, "claude-haiku-4-5", 10, 5, 1.0))?;
    assert!(!blocked.admitted, "Block rule rejects an over-cap event");
    assert!(
        blocked.statuses.iter().any(|s| s.rejects_ingest()),
        "rejection carries a breached enforcing status"
    );

    // The rejected event was never recorded: usage stays at the two admitted events.
    let u = store.usage_since(&pid, Utc::now() - chrono::Duration::hours(1))?;
    assert_eq!(u.calls, 2, "only the two admitted events are recorded");
    assert!((u.cost_usd - 2.0).abs() < 1e-9, "rejected event's cost not counted");
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
    assert_eq!(listed.len(), 2, "both point-in-time revenue records recognized in window");
    assert!(listed.iter().all(|r| r.project_id == pid), "list scoped to project");
    let got_acme = listed
        .iter()
        .find(|r| r.customer_id.as_deref() == Some("acme"))
        .expect("acme revenue present");
    assert!((got_acme.amount_usd - 20.0).abs() < 1e-9, "amount round-trip");
    assert_eq!(got_acme.external_id.as_deref(), Some("inv-acme"), "external_id round-trip");
    assert_eq!(got_acme.kind, RevenueKind::OneTime, "kind round-trip");

    // A replayed Stripe webhook: `normalize_invoice` runs again on the redelivery and yields a *fresh*
    // record carrying the same deterministic id (`stripe:<external_id>`). The upsert must collapse it
    // onto the existing row — a second physical row here would silently double every downstream margin
    // number, the exact corruption profit tracking exists to prevent.
    store.insert_revenue_event(&mk_rev("acme", 20.0))?;
    let after = store.list_revenue_events(Some(&pid), since, until)?;
    assert_eq!(after.len(), 2, "redelivered webhook upserts; total revenue row count unchanged");
    assert_eq!(
        after.iter().filter(|r| r.external_id.as_deref() == Some("inv-acme")).count(),
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
    assert!((acme_cost.cost_usd - 0.87).abs() < 1e-9, "acme cost summed across its events");
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
    let acme_row = rows.iter().find(|r| r.key == "acme").expect("acme margin row");
    assert!((acme_row.revenue_usd - 20.0).abs() < 1e-9, "revenue recognized once, not doubled");
    assert!((acme_row.gross_margin_usd - 19.13).abs() < 1e-9, "revenue − attributed cost");
    Ok(())
}

fn jobs(store: &dyn Store) -> Result<()> {
    let now = Utc::now();
    let j = Job {
        id: new_id(),
        job_type: "conf".into(),
        payload: json!({ "k": "v" }),
        status: "queued".into(),
        attempts: 0,
        max_attempts: 3,
        progress: None,
        error: None,
        result: Value::Null,
        claimed_at: None,
        created_at: now,
        updated_at: now,
    };
    store.create_job(&j)?;
    assert_eq!(store.get_job(&j.id)?.expect("get_job Some").status, "queued");

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
    assert!(store.list_jobs(Some("done"), 100)?.iter().any(|x| x.id == j.id));
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
        Ok(store.lease_relay_tasks("conf-dev", 60, 20)?.into_iter().find(|t| t.id == id))
    }

    let mut t = task(pid, 2);
    t.idempotency_key = Some(new_id());
    match store.create_relay_task(&t) {
        Err(e) if e.to_string().contains("not supported") => {
            eprintln!("skipping relay conformance: {e}");
            return Ok(());
        }
        r => r?,
    }

    // Round-trip + idempotency lookup.
    let got = store.get_relay_task(&t.id)?.expect("get_relay_task Some");
    assert_eq!(got.payload, json!({ "k": "v" }), "relay payload round-trip");
    let key = t.idempotency_key.clone().unwrap();
    assert_eq!(store.find_relay_task_by_key(pid, &key)?.expect("by key").id, t.id);
    assert!(store.find_relay_task_by_key("other-project", &key)?.is_none());

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
            &RelayOutcome::Deferred { retry_after_secs: Some(0), reason: Some("window".into()) },
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
    assert_eq!(done.result, json!({ "ok": true }), "relay result round-trip");
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
    assert!(held.iter().any(|x| x.id == vanished.id), "vanished task leased");
    let swept = store.sweep_relay_dead()?;
    let ours = swept.iter().find(|x| x.id == vanished.id).expect("sweep returns our task");
    assert_eq!(ours.status, "dead");
    assert_eq!(ours.error.as_deref(), Some("lease expired without a result"));
    Ok(())
}
