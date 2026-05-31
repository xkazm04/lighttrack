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
    new_id, ApiKey, Benchmark, BenchmarkCase, BenchmarkRun, Dataset, DatasetItem, Job, LimitAction,
    LimitMetric, LimitRule, LimitWindow, LlmEvent, ModelPriceRow, Operation, Project, Provider,
    Redaction, Rubric, RubricDimension, Score, Status, TokenUsage,
};

use crate::{Result, Store};

/// Run the full conformance suite against `store` (assumed already schema-initialized by its
/// constructor). Panics on a failed assertion; returns `Err` on a backend error.
pub fn run(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    events(store, &pid)?;
    projects_keys_limits(store, &pid)?;
    scores(store, &pid)?;
    prices(store)?;
    benchmarks(store, &pid)?;
    datasets(store, &pid)?;
    rubrics(store, &pid)?;
    jobs(store)?;
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
        created_at: Utc::now(),
    };
    store.create_project(&proj)?;
    assert!(store.get_project(pid)?.is_some(), "get_project Some");
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
