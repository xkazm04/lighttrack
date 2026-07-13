use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::Value;

use lighttrack_core::{
    new_id, ApiKey, Job, LimitAction, LimitMetric, LimitRule, LimitWindow, LlmEvent, Operation,
    Project, Prompt, PromptVersion, Provider, Redaction, Status, TokenUsage,
};

use super::SqliteStore;
use crate::Store;

fn ev(project: &str, model: &str, inp: u64, out: u64, cost: f64) -> LlmEvent {
    LlmEvent {
        id: new_id(),
        project_id: project.into(),
        trace_id: Some("trace-1".into()),
        span_id: None,
        parent_span_id: None,
        ts: Utc::now(),
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
        latency_ms: Some(123),
        status: Status::Success,
        error: None,
        input: None,
        output: None,
        tags: vec!["smoke".into()],
        source: Some("test".into()),
        metadata: serde_json::json!({"k":"v"}),
    }
}

#[test]
fn duplicate_event_id_is_a_conflict_not_an_opaque_error() {
    let s = SqliteStore::open_in_memory().unwrap();
    let mut e = ev("p1", "claude-haiku-4-5", 10, 5, 0.001);
    e.id = "fixed-id".into();
    s.insert_event(&e).unwrap();

    // Re-inserting the same id hits the PK and must surface as a typed `Conflict`, not `Sqlite`/`Other`.
    let err = s.insert_event(&e).unwrap_err();
    assert!(
        matches!(err, crate::StoreError::Conflict(_)),
        "duplicate id should map to Conflict, got {err:?}"
    );
    // The admission path guards the same insert, so a duplicate id through it is a Conflict too.
    let err2 = s.insert_event_checked(&e).unwrap_err();
    assert!(matches!(err2, crate::StoreError::Conflict(_)), "got {err2:?}");
    // Only the one row exists.
    assert_eq!(s.list_events(Some("p1"), 10).unwrap().len(), 1);
}

#[test]
fn keyset_pagination_is_stable_across_interleaved_inserts() {
    use chrono::{TimeZone, Utc as ChronoUtc};
    let s = SqliteStore::open_in_memory().unwrap();
    // Five events, strictly increasing ts (so DESC order is e5,e4,e3,e2,e1).
    let mk = |n: u32| {
        let mut e = ev("p1", "m", 1, 1, 0.0);
        e.id = format!("e{n}");
        e.ts = ChronoUtc.with_ymd_and_hms(2026, 1, 1, 0, 0, n).unwrap();
        e
    };
    for n in 1..=5 {
        s.insert_event(&mk(n)).unwrap();
    }

    let filter = crate::EventFilter::default();
    let page1 = s.list_events_filtered(Some("p1"), &filter, 2).unwrap();
    assert_eq!(
        page1.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["e5", "e4"]
    );
    let c1 = page1.next_cursor.expect("more rows remain");

    // Insert a brand-new, newest event mid-pagination. Keyset paging must not shift the window: the
    // next page continues strictly below e4, unaffected by e6.
    s.insert_event(&mk(6)).unwrap();

    let f2 = crate::EventFilter { cursor: Some(c1), ..Default::default() };
    let page2 = s.list_events_filtered(Some("p1"), &f2, 2).unwrap();
    assert_eq!(
        page2.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["e3", "e2"]
    );
    let c2 = page2.next_cursor.expect("one more row remains");

    let f3 = crate::EventFilter { cursor: Some(c2), ..Default::default() };
    let page3 = s.list_events_filtered(Some("p1"), &f3, 2).unwrap();
    assert_eq!(page3.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1"]);
    assert!(page3.next_cursor.is_none(), "last page has no cursor");
    // No duplicates, no skips across the session (e6 correctly excluded — it's newer than the cursor).
}

#[test]
fn filtered_listing_ands_all_predicates() {
    let s = SqliteStore::open_in_memory().unwrap();
    let mut a = ev("p1", "claude-haiku-4-5", 1, 1, 0.0);
    a.id = "a".into();
    a.provider = Provider::Anthropic;
    a.name = Some("summarize".into());
    a.trace_id = Some("t-a".into());
    let mut b = ev("p1", "gpt-4o", 1, 1, 0.0);
    b.id = "b".into();
    b.provider = Provider::OpenAi;
    b.name = Some("classify".into());
    b.trace_id = Some("t-b".into());
    s.insert_event(&a).unwrap();
    s.insert_event(&b).unwrap();

    let by_provider = crate::EventFilter { provider: Some("openai".into()), ..Default::default() };
    let r = s.list_events_filtered(Some("p1"), &by_provider, 50).unwrap();
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].id, "b");

    let by_model_and_name = crate::EventFilter {
        model: Some("claude-haiku-4-5".into()),
        name: Some("summarize".into()),
        ..Default::default()
    };
    let r = s.list_events_filtered(Some("p1"), &by_model_and_name, 50).unwrap();
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].id, "a");

    // Contradictory predicates → empty.
    let none = crate::EventFilter {
        provider: Some("openai".into()),
        name: Some("summarize".into()),
        ..Default::default()
    };
    assert!(s.list_events_filtered(Some("p1"), &none, 50).unwrap().events.is_empty());
}

#[test]
fn windowed_filter_and_cost_summary_respect_since_inclusive_until_exclusive() {
    use chrono::{TimeZone, Utc as ChronoUtc};
    let s = SqliteStore::open_in_memory().unwrap();
    let at = |h: u32, id: &str| {
        let mut e = ev("p1", "m", 1, 1, 1.0);
        e.id = id.into();
        e.ts = ChronoUtc.with_ymd_and_hms(2026, 1, 1, h, 0, 0).unwrap();
        e
    };
    for (h, id) in [(1, "h1"), (2, "h2"), (3, "h3")] {
        s.insert_event(&at(h, id)).unwrap();
    }
    let since = ChronoUtc.with_ymd_and_hms(2026, 1, 1, 2, 0, 0).unwrap();
    let until = ChronoUtc.with_ymd_and_hms(2026, 1, 1, 3, 0, 0).unwrap();

    // [since, until): h2 only (h1 before since, h3 at until is excluded).
    let f = crate::EventFilter { since: Some(since), until: Some(until), ..Default::default() };
    let r = s.list_events_filtered(Some("p1"), &f, 50).unwrap();
    assert_eq!(r.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["h2"]);

    let costs = s.cost_summary_windowed(Some("p1"), Some(since), Some(until)).unwrap();
    assert_eq!(costs.len(), 1);
    assert_eq!(costs[0].calls, 1, "only h2 falls in the window");

    // No bounds == full history (matches cost_summary).
    let all = s.cost_summary_windowed(Some("p1"), None, None).unwrap();
    assert_eq!(all[0].calls, 3);
}

#[test]
fn insert_list_cost_roundtrip() {
    let s = SqliteStore::open_in_memory().unwrap();
    s.insert_event(&ev("p1", "claude-haiku-4-5", 100, 50, 0.001)).unwrap();
    s.insert_event(&ev("p1", "claude-haiku-4-5", 200, 80, 0.002)).unwrap();
    s.insert_event(&ev("p2", "claude-opus-4-8", 10, 5, 0.01)).unwrap();

    assert_eq!(s.list_events(None, 10).unwrap().len(), 3);
    let p1 = s.list_events(Some("p1"), 10).unwrap();
    assert_eq!(p1.len(), 2);
    assert_eq!(p1[0].project_id, "p1");
    assert_eq!(p1[0].tags, vec!["smoke".to_string()]);
    assert_eq!(p1[0].metadata, serde_json::json!({"k":"v"}));

    let costs = s.cost_summary(Some("p1")).unwrap();
    assert_eq!(costs.len(), 1);
    assert_eq!(costs[0].calls, 2);
    assert_eq!(costs[0].input_tokens, 300);
    assert!((costs[0].cost_usd - 0.003).abs() < 1e-9);
}

#[test]
fn usecase_costs_groups_by_name_with_fallback_and_window() {
    let s = SqliteStore::open_in_memory().unwrap();

    // Two named use-cases (one with 2 calls) + one un-named call, all in p1.
    let mut a1 = ev("p1", "claude-haiku-4-5", 100, 50, 0.001);
    a1.name = Some("summarize".into());
    let mut a2 = ev("p1", "claude-haiku-4-5", 200, 80, 0.002);
    a2.name = Some("summarize".into());
    let mut b1 = ev("p1", "claude-opus-4-8", 10, 5, 0.01);
    b1.name = Some("classify".into());
    let unnamed = ev("p1", "gpt-4o-mini", 5, 5, 0.0001); // name: None → model-fallback bucket
    for e in [&a1, &a2, &b1, &unnamed] {
        s.insert_event(e).unwrap();
    }

    // Grouped by (name, provider, model): summarize + classify + the un-named bucket = 3 rows.
    let rows = s.usecase_costs(Some("p1"), None).unwrap();
    assert_eq!(rows.len(), 3);
    let summarize = rows.iter().find(|r| r.name.as_deref() == Some("summarize")).unwrap();
    assert_eq!(summarize.calls, 2);
    assert_eq!(summarize.input_tokens, 300);
    assert_eq!(summarize.model, "claude-haiku-4-5");
    assert!((summarize.cost_usd - 0.003).abs() < 1e-9);
    // The un-named call rolls up under a name=None row (keyed by its model).
    assert!(rows.iter().any(|r| r.name.is_none() && r.model == "gpt-4o-mini"));

    // Windowing: a call stamped 10 days ago is excluded by a 7-day `since`.
    let mut old = ev("p1", "claude-haiku-4-5", 1, 1, 0.5);
    old.name = Some("summarize".into());
    old.ts = Utc::now() - chrono::Duration::days(10);
    s.insert_event(&old).unwrap();
    let since = Utc::now() - chrono::Duration::days(7);
    let windowed = s.usecase_costs(Some("p1"), Some(since)).unwrap();
    let summ_win = windowed.iter().find(|r| r.name.as_deref() == Some("summarize")).unwrap();
    assert_eq!(summ_win.calls, 2, "the 10-day-old call is outside the 7-day window");
}

#[test]
fn trace_rollup_groups_events_and_scores() {
    use lighttrack_core::Score;

    let s = SqliteStore::open_in_memory().unwrap();

    // A two-span trace for p1: a root call + a child call. Give them distinct span ids/parents.
    let mut root = ev("p1", "claude-haiku-4-5", 100, 50, 0.001);
    root.trace_id = Some("tr-1".into());
    root.span_id = Some("s-root".into());
    root.parent_span_id = None;
    let mut child = ev("p1", "claude-opus-4-8", 200, 80, 0.004);
    child.trace_id = Some("tr-1".into());
    child.span_id = Some("s-child".into());
    child.parent_span_id = Some("s-root".into());
    // An unrelated single-span trace, and a trace for another project.
    let mut other = ev("p1", "claude-haiku-4-5", 10, 5, 0.0005);
    other.trace_id = Some("tr-2".into());
    let mut foreign = ev("p2", "claude-haiku-4-5", 10, 5, 0.01);
    foreign.trace_id = Some("tr-3".into());
    for e in [&root, &child, &other, &foreign] {
        s.insert_event(e).unwrap();
    }

    // Listing is per-project and one row per trace, newest first.
    let traces = s.list_traces(Some("p1"), 10).unwrap();
    assert_eq!(traces.len(), 2, "two distinct p1 traces");
    let t1 = traces.iter().find(|t| t.trace_id == "tr-1").expect("tr-1 present");
    assert_eq!(t1.spans, 2);
    assert!((t1.cost_usd - 0.005).abs() < 1e-9);
    assert_eq!(t1.total_tokens, 430);
    assert_eq!(t1.models.len(), 2, "two distinct models in the trace");

    // The rollup nests child under root and totals the trace.
    let trace = s.get_trace("tr-1").unwrap().expect("get_trace Some");
    assert_eq!(trace.totals.spans, 2);
    assert_eq!(trace.spans.len(), 1, "single root span");
    assert_eq!(trace.spans[0].children.len(), 1, "child nests under root");
    assert!(s.get_trace("nope").unwrap().is_none(), "unknown trace -> None");

    // A per-call score on the child + a whole-trace score anchored to the root both surface via join.
    let mk_score = |event_id: &str, rubric: &str| Score {
        id: new_id(),
        project_id: "p1".into(),
        event_id: Some(event_id.into()),
        rubric: rubric.into(),
        value: 0.8,
        max: 1.0,
        pass: Some(true),
        reasoning: None,
        scored_by: "judge".into(),
        cost_usd: Some(0.0001),
        created_at: Utc::now(),
    };
    s.insert_score(&mk_score(&child.id, "call-quality")).unwrap();
    s.insert_score(&mk_score(&root.id, "trace-coherence")).unwrap();
    let scores = s.list_trace_scores("tr-1").unwrap();
    assert_eq!(scores.len(), 2, "both the per-call and whole-trace scores join to the trace");
}

#[test]
fn projects_keys_limits_usage() {
    let s = SqliteStore::open_in_memory().unwrap();
    let now = Utc::now();

    let proj = Project {
        id: "p1".into(),
        name: "demo".into(),
        enabled: true,
        redaction: Redaction::None,
        created_at: now,
    };
    s.create_project(&proj).unwrap();
    assert_eq!(s.list_projects().unwrap().len(), 1);
    assert!(s.get_project("p1").unwrap().is_some());
    assert!(s.get_project("nope").unwrap().is_none());

    let key = ApiKey {
        id: "k1".into(),
        project_id: "p1".into(),
        name: "default".into(),
        prefix: "abc12345".into(),
        key_hash: "salt:hash".into(),
        created_at: now,
        last_used_at: None,
        revoked: false,
    };
    s.create_api_key(&key).unwrap();
    assert_eq!(s.find_api_key_by_prefix("abc12345").unwrap().unwrap().project_id, "p1");
    assert!(s.find_api_key_by_prefix("zzz").unwrap().is_none());

    let rule = LimitRule {
        id: "r1".into(),
        project_id: "p1".into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: 0.005,
        action: LimitAction::Alert,
        enabled: true,
    };
    s.create_limit_rule(&rule).unwrap();
    assert_eq!(s.list_limit_rules("p1", true).unwrap().len(), 1);

    s.insert_event(&ev("p1", "claude-haiku-4-5", 1000, 500, 0.0035)).unwrap();
    s.insert_event(&ev("p1", "claude-haiku-4-5", 2000, 200, 0.00165)).unwrap();

    let u = s.usage_since("p1", LimitWindow::Hour.since(Utc::now())).unwrap();
    assert_eq!(u.calls, 2);
    assert_eq!(u.tokens, 3700);
    assert!((u.cost_usd - 0.00515).abs() < 1e-9);
    assert!(rule.evaluate(u.cost_usd).breached);
}

#[test]
fn insert_event_checked_enforces_caps() {
    let s = SqliteStore::open_in_memory().unwrap();

    // No rules: admitted and recorded.
    let a = s.insert_event_checked(&ev("p1", "claude-haiku-4-5", 100, 50, 1.0)).unwrap();
    assert!(a.admitted);
    assert!(a.statuses.is_empty());

    // Block on calls, threshold 2: the 2nd call would push usage-with-this-event to 2 >= 2 -> reject.
    s.create_limit_rule(&LimitRule {
        id: "r-block".into(),
        project_id: "p1".into(),
        metric: LimitMetric::Calls,
        window: LimitWindow::Hour,
        threshold: 2.0,
        action: LimitAction::Block,
        enabled: true,
    })
    .unwrap();
    let blocked = s.insert_event_checked(&ev("p1", "claude-haiku-4-5", 1, 1, 0.0)).unwrap();
    assert!(!blocked.admitted, "Block rejects the over-cap event");
    assert!(blocked.statuses.iter().any(|st| st.rejects_ingest()));

    // The rejected event is not recorded: still exactly one event for p1.
    assert_eq!(s.list_events(Some("p1"), 10).unwrap().len(), 1, "rejected event not persisted");
    let u = s.usage_since("p1", LimitWindow::Hour.since(Utc::now())).unwrap();
    assert_eq!(u.calls, 1);

    // A different project is unaffected by p1's cap.
    let other = s.insert_event_checked(&ev("p2", "claude-haiku-4-5", 1, 1, 0.0)).unwrap();
    assert!(other.admitted, "limits are per-project");
}

#[test]
fn insert_event_checked_alert_never_blocks() {
    let s = SqliteStore::open_in_memory().unwrap();
    s.create_limit_rule(&LimitRule {
        id: "r-alert".into(),
        project_id: "p1".into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: 0.001,
        action: LimitAction::Alert,
        enabled: true,
    })
    .unwrap();
    // Way over the Alert threshold, but Alert is observe-only: admitted + recorded, breach reported.
    let a = s.insert_event_checked(&ev("p1", "claude-haiku-4-5", 100, 50, 5.0)).unwrap();
    assert!(a.admitted, "Alert action does not block");
    assert!(a.statuses.iter().any(|st| st.breached), "breach is still reported");
    assert!(!a.statuses.iter().any(|st| st.rejects_ingest()), "Alert breach never rejects");
    assert_eq!(s.list_events(Some("p1"), 10).unwrap().len(), 1);
}

#[test]
fn job_queue_claim_finish() {
    let s = SqliteStore::open_in_memory().unwrap();
    let now = Utc::now();
    let job = Job {
        id: "j1".into(),
        job_type: "bench_run".into(),
        payload: serde_json::json!({ "benchmark_id": "b1" }),
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
    s.create_job(&job).unwrap();

    let claimed = s.claim_job(now).unwrap().unwrap();
    assert_eq!(claimed.id, "j1");
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.attempts, 1);
    assert_eq!(claimed.payload["benchmark_id"], "b1");

    assert!(s.claim_job(now - chrono::Duration::seconds(1)).unwrap().is_none());

    s.finish_job("j1", "done", &serde_json::json!({ "run_id": "r1" }), None).unwrap();
    let got = s.get_job("j1").unwrap().unwrap();
    assert_eq!(got.status, "done");
    assert_eq!(got.result["run_id"], "r1");
    assert_eq!(s.list_jobs(Some("done"), 10).unwrap().len(), 1);
}

#[test]
fn prompt_registry_versions_and_labels() {
    let s = SqliteStore::open_in_memory().unwrap();
    let now = Utc::now();
    let prompt = Prompt {
        id: "pr1".into(),
        project_id: "p1".into(),
        name: "support-reply".into(),
        benchmark_id: Some("b1".into()),
        labels: BTreeMap::new(),
        created_at: now,
        updated_at: now,
    };
    s.create_prompt(&prompt).unwrap();

    // Two immutable versions.
    for (v, body) in [(1u32, "v1 text"), (2u32, "v2 text")] {
        s.create_prompt_version(&PromptVersion {
            id: new_id(),
            prompt_id: "pr1".into(),
            version: v,
            content: body.into(),
            config: serde_json::json!({ "model": "haiku" }),
            note: Some(format!("cut {v}")),
            created_at: now,
        })
        .unwrap();
    }

    // Lookup by project+name (the runtime fetch path) and by id.
    let by_name = s.get_prompt("p1", "support-reply").unwrap().unwrap();
    assert_eq!(by_name.id, "pr1");
    assert_eq!(by_name.benchmark_id.as_deref(), Some("b1"));
    assert!(s.get_prompt_by_id("pr1").unwrap().is_some());
    assert!(s.get_prompt("p1", "missing").unwrap().is_none());

    // Versions: newest first, config + note round-trip.
    let versions = s.list_prompt_versions("pr1").unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version, 2);
    assert_eq!(versions[0].config, serde_json::json!({ "model": "haiku" }));
    let v1 = s.get_prompt_version("pr1", 1).unwrap().unwrap();
    assert_eq!(v1.content, "v1 text");
    assert_eq!(v1.note.as_deref(), Some("cut 1"));

    // Promote a label, then read it back.
    let mut updated = by_name;
    updated.labels.insert("production".into(), 2);
    updated.updated_at = Utc::now();
    s.update_prompt(&updated).unwrap();
    let reloaded = s.get_prompt("p1", "support-reply").unwrap().unwrap();
    assert_eq!(reloaded.labels.get("production"), Some(&2));
    assert!(s.list_prompts("p1").unwrap().iter().any(|p| p.id == "pr1"));
}

/// A relay task due immediately, with a zero retry interval so failure requeues are leasable
/// right away in tests.
fn relay_task(project: &str, action: &str, max_attempts: u32) -> lighttrack_core::RelayTask {
    let now = Utc::now();
    lighttrack_core::RelayTask {
        id: new_id(),
        project_id: project.into(),
        source: Some("test".into()),
        action_type: action.into(),
        payload: serde_json::json!({ "sku": "A-1" }),
        status: "queued".into(),
        attempts: 0,
        max_attempts,
        retry_interval_secs: 0,
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

#[test]
fn relay_lease_settle_success_roundtrip() {
    use lighttrack_core::RelayOutcome;

    let s = SqliteStore::open_in_memory().unwrap();
    let t = relay_task("p1", "xprice/summary", 4);
    s.create_relay_task(&t).unwrap();

    let leased = s.lease_relay_tasks("dev-1", 600, 5).unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].status, "leased");
    assert_eq!(leased[0].attempts, 1);
    assert_eq!(leased[0].device.as_deref(), Some("dev-1"));
    assert_eq!(leased[0].payload["sku"], "A-1");

    // Held lease is not re-leasable.
    assert!(s.lease_relay_tasks("dev-1", 600, 5).unwrap().is_empty());

    let done = s
        .settle_relay_task(&t.id, &RelayOutcome::Succeeded(serde_json::json!({ "ok": true })))
        .unwrap()
        .unwrap();
    assert_eq!(done.status, "succeeded");
    assert_eq!(done.result["ok"], true);

    // A duplicate result report is harmless: the settled row comes back unchanged.
    let again = s
        .settle_relay_task(&t.id, &RelayOutcome::Failed("late duplicate".into()))
        .unwrap()
        .unwrap();
    assert_eq!(again.status, "succeeded");
}

#[test]
fn relay_failure_requeues_then_dead_letters() {
    use lighttrack_core::RelayOutcome;

    let s = SqliteStore::open_in_memory().unwrap();
    let t = relay_task("p1", "xprice/summary", 2);
    s.create_relay_task(&t).unwrap();

    // Attempt 1 fails → back to queued (zero interval ⇒ due immediately), error recorded.
    s.lease_relay_tasks("dev-1", 600, 1).unwrap();
    let requeued = s
        .settle_relay_task(&t.id, &RelayOutcome::Failed("boom".into()))
        .unwrap()
        .unwrap();
    assert_eq!(requeued.status, "queued");
    assert_eq!(requeued.attempts, 1);
    assert_eq!(requeued.error.as_deref(), Some("boom"));

    // Attempt 2 fails → attempts exhausted → dead.
    assert_eq!(s.lease_relay_tasks("dev-1", 600, 1).unwrap().len(), 1);
    let dead = s
        .settle_relay_task(&t.id, &RelayOutcome::Failed("boom again".into()))
        .unwrap()
        .unwrap();
    assert_eq!(dead.status, "dead");
    assert_eq!(dead.attempts, 2);
}

#[test]
fn relay_deferred_hands_the_attempt_back() {
    use lighttrack_core::RelayOutcome;

    let s = SqliteStore::open_in_memory().unwrap();
    let t = relay_task("p1", "xprice/summary", 1);
    s.create_relay_task(&t).unwrap();

    s.lease_relay_tasks("dev-1", 600, 1).unwrap();
    let deferred = s
        .settle_relay_task(
            &t.id,
            &RelayOutcome::Deferred {
                retry_after_secs: Some(0),
                reason: Some("subscription window exhausted".into()),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(deferred.status, "queued");
    assert_eq!(deferred.attempts, 0); // handed back — deferral never burns an attempt

    // Still leasable despite max_attempts = 1.
    let released = s.lease_relay_tasks("dev-1", 600, 1).unwrap();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].attempts, 1);
}

#[test]
fn relay_expired_lease_is_reclaimed_or_dead_lettered() {
    let s = SqliteStore::open_in_memory().unwrap();
    // Two tasks: one with attempts to spare, one on its last attempt.
    let spare = relay_task("p1", "a/retry", 2);
    let last = relay_task("p1", "a/last", 1);
    s.create_relay_task(&spare).unwrap();
    s.create_relay_task(&last).unwrap();

    // Zero-second leases expire immediately (the device "vanished").
    assert_eq!(s.lease_relay_tasks("dev-1", 0, 5).unwrap().len(), 2);

    // The sweep dead-letters the exhausted task (and returns it, for alerting) …
    let dead = s.sweep_relay_dead().unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].id, last.id);
    assert_eq!(dead[0].status, "dead");
    assert_eq!(dead[0].error.as_deref(), Some("lease expired without a result"));
    assert!(s.sweep_relay_dead().unwrap().is_empty()); // idempotent

    // … while the one with attempts to spare is re-leased on attempt 2.
    let reclaimed = s.lease_relay_tasks("dev-2", 600, 5).unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].id, spare.id);
    assert_eq!(reclaimed[0].attempts, 2);
    assert_eq!(reclaimed[0].device.as_deref(), Some("dev-2"));
}

#[test]
fn relay_idempotency_key_is_unique_per_project() {
    let s = SqliteStore::open_in_memory().unwrap();
    let mut t = relay_task("p1", "xprice/summary", 4);
    t.idempotency_key = Some("order-42".into());
    s.create_relay_task(&t).unwrap();

    let found = s.find_relay_task_by_key("p1", "order-42").unwrap().unwrap();
    assert_eq!(found.id, t.id);
    assert!(s.find_relay_task_by_key("p2", "order-42").unwrap().is_none());

    // Same (project, key) again violates the partial unique index.
    let mut dup = relay_task("p1", "xprice/summary", 4);
    dup.idempotency_key = Some("order-42".into());
    assert!(s.create_relay_task(&dup).is_err());

    // Listing filters by project and status.
    let other = relay_task("p2", "b/other", 4);
    s.create_relay_task(&other).unwrap();
    assert_eq!(s.list_relay_tasks(None, None, 10).unwrap().len(), 2);
    assert_eq!(s.list_relay_tasks(Some("p1"), None, 10).unwrap().len(), 1);
    assert_eq!(s.list_relay_tasks(Some("p1"), Some("queued"), 10).unwrap().len(), 1);
    assert_eq!(s.list_relay_tasks(Some("p1"), Some("dead"), 10).unwrap().len(), 0);
}
