//! Router-level tests for stored schedules and the sweep that fires them.
//!
//! The property worth proving is the one the design set out to fix: a **compare** benchmark — a
//! matrix `target`, which physically cannot carry the old `schedule_interval_secs` key — recurs
//! through exactly the same mechanism as everything else, and `GET /v1/schedules` names it.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};
use lighttrack_store::Scope as TenantScope;

/// The admin bearer `tests_ingest::setup` configures (its state is auth-ENFORCED, not dev mode).
const ADMIN: &str = "admin-secret";

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

/// A compare benchmark: its `target` is a MATRIX, which is exactly the shape that could not carry
/// `target.schedule_interval_secs` — so before M7 the headline benchmark mode could not recur.
fn compare_benchmark(
    store: &std::sync::Arc<lighttrack_store::SqliteStore>,
    project: &str,
) -> String {
    // Materialize the project row too: the deployment-wide listing walks projects, so a schedule
    // hanging off a project that does not exist would be invisible — which is the right behaviour
    // (nothing owns it) but not the case under test here.
    make_key(store, project);
    let b: lighttrack_core::Benchmark = serde_json::from_value(json!({
        "project_id": project,
        "name": "matrix-compare",
        "rubric": "helpfulness",
        "target": [
            { "provider": "openai", "model": "gpt-5" },
            { "provider": "anthropic", "model": "claude-sonnet-5" }
        ]
    }))
    .unwrap();
    store.create_benchmark(&b).unwrap();
    b.id
}

#[tokio::test]
async fn a_compare_benchmark_recurs_and_the_sweep_never_stacks_two_of_it() {
    let (state, store) = setup(Redactor::off());
    let admin = ADMIN;
    let app = crate::build_router(state.clone());
    let bid = compare_benchmark(&store, "proj-a");

    let (status, sched) = call(
        &app,
        "POST",
        "/v1/projects/proj-a/schedules",
        admin,
        Some(json!({ "type": "bench_run", "interval_secs": 3600,
                     "payload": { "benchmark_id": bid, "samples": 2 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sid = sched["id"].as_str().unwrap().to_string();
    assert_eq!(sched["enabled"], true);

    // Listed, deployment-wide — the answer to "what runs on a schedule here".
    let (status, all) = call(&app, "GET", "/v1/schedules", admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(all
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == sid.as_str()));

    // One sweep enqueues one job, stamped with the schedule that produced it.
    crate::schedule_sweep::sweep_once(&state).await;
    let jobs = store
        .list_jobs(TenantScope::Operator, Some("queued"), 100)
        .unwrap();
    assert_eq!(jobs.len(), 1, "one due schedule, one job");
    assert_eq!(jobs[0].job_type, "bench_run");
    assert_eq!(jobs[0].payload["benchmark_id"], bid.as_str());
    assert_eq!(jobs[0].payload["samples"], 2);
    assert_eq!(jobs[0].payload["schedule_id"], sid.as_str());

    // A second sweep must NOT stack another: the first job is still in flight. This is the
    // idempotency rule that keeps a benchmark slower than its own interval from piling up runs.
    crate::schedule_sweep::sweep_once(&state).await;
    assert_eq!(
        store
            .list_jobs(TenantScope::Operator, Some("queued"), 100)
            .unwrap()
            .len(),
        1
    );

    // …and the schedule now points at the run it produced.
    let (_, runs) = call(
        &app,
        "GET",
        &format!("/v1/schedules/{sid}/runs"),
        admin,
        None,
    )
    .await;
    assert_eq!(runs.as_array().unwrap().len(), 1);
    assert_eq!(runs[0]["id"], jobs[0].id.as_str());
}

#[tokio::test]
async fn a_schedule_that_could_only_enqueue_rejects_is_refused_at_the_door() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    // Unknown kind → 400 naming the vocabulary, not a stored schedule nothing can ever run.
    let (status, err) = call(
        &app,
        "POST",
        "/v1/projects/proj-a/schedules",
        ADMIN,
        Some(json!({ "type": "bench-run", "interval_secs": 3600 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("bench_run"),
        "the refusal must name what IS accepted: {err}"
    );

    // Known kind, payload missing the one field with no defensible default → also 400.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/projects/proj-a/schedules",
        ADMIN,
        Some(json!({ "type": "bench_run", "interval_secs": 3600, "payload": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The same door on POST /v1/jobs, for the external-scheduler path.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/jobs",
        ADMIN,
        Some(json!({ "type": "score_events", "payload": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no rubric contract");
    let (status, job) = call(
        &app,
        "POST",
        "/v1/jobs",
        ADMIN,
        Some(json!({ "type": "score_events", "payload": { "rubric": "be helpful" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(job["type"], "score_events");
}

#[tokio::test]
async fn a_disabled_schedule_stays_visible_and_stops_firing() {
    let (state, store) = setup(Redactor::off());
    let app = crate::build_router(state.clone());
    let bid = compare_benchmark(&store, "proj-a");

    let (_, sched) = call(
        &app,
        "POST",
        "/v1/projects/proj-a/schedules",
        ADMIN,
        Some(json!({ "type": "bench_run", "interval_secs": 60,
                     "payload": { "benchmark_id": bid } })),
    )
    .await;
    let sid = sched["id"].as_str().unwrap().to_string();

    // Pausing must not rewrite the payload — an absent field means "leave it".
    let (status, off) = call(
        &app,
        "PUT",
        &format!("/v1/schedules/{sid}"),
        ADMIN,
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(off["enabled"], false);
    assert_eq!(off["payload"]["benchmark_id"], bid.as_str());

    crate::schedule_sweep::sweep_once(&state).await;
    assert!(
        store
            .list_jobs(TenantScope::Operator, Some("queued"), 100)
            .unwrap()
            .is_empty(),
        "a disabled schedule must not fire"
    );
    // Paused is not deleted: an operator has to be able to see the thing they paused.
    let (status, listed) = call(&app, "GET", "/v1/projects/proj-a/schedules", ADMIN, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, _) = call(&app, "DELETE", &format!("/v1/schedules/{sid}"), ADMIN, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&app, "DELETE", &format!("/v1/schedules/{sid}"), ADMIN, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a second delete finds nothing"
    );
}

/// A device that vanishes leaves tasks nobody is polling for — which is precisely when the old
/// reap, hosted inside `lease_relay_tasks`, could never run. The timed sweep is what closes that.
#[tokio::test]
async fn the_sweep_reaps_dead_relay_leases_with_no_device_polling() {
    let (state, store) = setup(Redactor::off());
    let app = crate::build_router(state.clone());

    let (_, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        ADMIN,
        Some(
            json!({ "action_type": "xprice/summary", "project_id": "proj-a",
                     "max_attempts": 1 }),
        ),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();

    // Burn the retry budget, then strand the task under an already-expired lease.
    store.lease_relay_tasks("pc", &[], 0, 5).unwrap();
    for _ in 0..lighttrack_core::RELAY_MAX_STALE_RECLAIMS {
        store.lease_relay_tasks("pc", &[], 0, 5).unwrap();
    }
    assert_eq!(
        store
            .get_relay_task(TenantScope::Operator, &id)
            .unwrap()
            .unwrap()
            .status,
        "leased",
        "still leased by a device that will never come back"
    );

    // No device polls; the sweep alone reaps it.
    crate::schedule_sweep::sweep_once(&state).await;
    assert_eq!(
        store
            .get_relay_task(TenantScope::Operator, &id)
            .unwrap()
            .unwrap()
            .status,
        "dead"
    );
}
