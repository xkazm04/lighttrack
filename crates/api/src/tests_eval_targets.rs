//! End-to-end over the wired router: a resolvable eval target, and the promotion gate that now
//! demands the run it certifies actually ran the version.
//!
//! The unit tests in `tests_prompt_gate` pin the policy; these pin that the policy is *reachable* —
//! that a benchmark carrying a `prompt_ref` really does turn a green run into a 409 when that run
//! never resolved anything, and that an `Http` target's URL is refused at the door rather than
//! discovered by the worker that POSTs to it.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::setup;

const ADMIN: &str = "admin-secret";

async fn send(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {ADMIN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn ok(app: &axum::Router, method: &str, uri: &str, body: Value) -> Value {
    let (st, v) = send(app, method, uri, body).await;
    assert_eq!(st, StatusCode::OK, "{method} {uri}: {v}");
    v
}

fn err_message(body: &Value) -> String {
    body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The full resolvable setup, in the order the chicken-and-egg forces: the prompt first (a
/// `prompt_ref` must name something that exists), the benchmark second, the link last.
async fn resolvable(app: &axum::Router, resolvable: bool) -> String {
    ok(
        app,
        "POST",
        "/v1/projects/p1/prompts",
        json!({ "name": "support-reply", "content": "you are terse" }),
    )
    .await;
    let mut target = json!({ "provider": "openai", "model": "gpt-4o" });
    if resolvable {
        target["prompt_ref"] = json!({ "name": "support-reply" });
    }
    let bench = ok(
        app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "support-quality", "rubric": "is it helpful?",
            "baseline_score": 0.80, "targets": [target]
        }),
    )
    .await;
    let bench_id = bench["id"].as_str().unwrap().to_string();
    ok(
        app,
        "PUT",
        "/v1/projects/p1/prompts/support-reply",
        json!({ "benchmark_id": bench_id }),
    )
    .await;
    ok(
        app,
        "POST",
        "/v1/projects/p1/prompts/support-reply/versions",
        json!({ "content": "you are terse and cite policy" }),
    )
    .await;
    bench_id
}

/// The prompt's id, which the run report must carry for the gate to find the run.
async fn prompt_id(app: &axum::Router) -> String {
    let list = ok(app, "GET", "/v1/projects/p1/prompts", json!({})).await;
    list[0]["id"].as_str().unwrap().to_string()
}

/// Record a finished, green run of `bench` tagged as having scored v2 of `pid`, merging `extra`
/// into its report.
///
/// Every call gets its own minute. The gate picks "the latest run that scored this version", so a
/// test asserting *which* run it picked has to give the runs an order — and this helper used to
/// stamp every run with the same hardcoded instant, which meant the only test that records two of
/// them was asserting an order the data never established. It passed on whatever sequence the
/// storage engine happened to return, and stopped passing the day `benchmark_runs` grew an index.
/// A monotonic stamp says what the scenario always meant: the second run happened afterwards.
async fn record_run(app: &axum::Router, bench: &str, pid: &str, extra: Value) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NTH: AtomicU32 = AtomicU32::new(0);
    let n = NTH.fetch_add(1, Ordering::Relaxed);

    let mut report = json!({ "prompt_id": pid, "prompt_version": 2 });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        report[k] = v;
    }
    ok(
        app,
        "POST",
        "/v1/benchmark-runs",
        json!({
            "benchmark_id": bench,
            "started_at": format!("2026-01-01T00:{n:02}:00.000000000Z"),
            "finished_at": format!("2026-01-01T00:{:02}:00.000000000Z", n + 1),
            "n_cases": 20, "mean_score": 0.95, "status": "passed",
            "report": report
        }),
    )
    .await;
}

#[tokio::test]
async fn a_green_run_that_never_resolved_the_version_cannot_promote_it() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    let bench = resolvable(&app, true).await;
    let pid = prompt_id(&app).await;

    // The exact failure M10 exists for: an EXCELLENT mean, tagged with the right prompt and the
    // right version — and no `resolved_prompt_version`, because the run generated from the target's
    // stored content and never read the registry. This used to promote.
    record_run(&app, &bench, &pid, json!({})).await;
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/prompts/support-reply/promote",
        json!({ "label": "production", "version": 2 }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(
        err_message(&body).contains("resolved_prompt_version"),
        "the refusal names the missing evidence: {body}"
    );

    // `force` is still the operator's escape hatch, as everywhere else in this gate.
    let forced = ok(
        &app,
        "POST",
        "/v1/projects/p1/prompts/support-reply/promote",
        json!({ "label": "staging", "version": 2, "force": true }),
    )
    .await;
    assert_eq!(forced["labels"]["staging"], 2);
    assert!(
        forced["warning"].is_null(),
        "a forced promotion is not a warning"
    );
}

#[tokio::test]
async fn a_run_that_resolved_the_version_promotes_it() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    let bench = resolvable(&app, true).await;
    let pid = prompt_id(&app).await;

    // Same run, now reporting the version it actually generated with.
    record_run(&app, &bench, &pid, json!({ "resolved_prompt_version": 2 })).await;
    let body = ok(
        &app,
        "POST",
        "/v1/projects/p1/prompts/support-reply/promote",
        json!({ "label": "production", "version": 2 }),
    )
    .await;
    assert_eq!(body["labels"]["production"], 2);
    assert!(body["warning"].is_null(), "nothing was left unchecked");

    // A run that resolved a DIFFERENT version is evidence about different content.
    let bench2 = bench.clone();
    record_run(&app, &bench2, &pid, json!({ "resolved_prompt_version": 1 })).await;
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/prompts/support-reply/promote",
        json!({ "label": "canary", "version": 2 }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(err_message(&body).contains("version 1"), "{body}");
}

#[tokio::test]
async fn a_benchmark_with_no_prompt_ref_warns_instead_of_blocking() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    // Same setup, except the target carries no `prompt_ref` — the shape every existing project has.
    let bench = resolvable(&app, false).await;
    let pid = prompt_id(&app).await;
    record_run(&app, &bench, &pid, json!({})).await;

    let body = ok(
        &app,
        "POST",
        "/v1/projects/p1/prompts/support-reply/promote",
        json!({ "label": "production", "version": 2 }),
    )
    .await;
    assert_eq!(
        body["labels"]["production"], 2,
        "working gates are not broken by this release"
    );
    let warning = body["warning"]
        .as_str()
        .expect("but the caveat is attached");
    assert!(
        warning.contains("prompt_ref"),
        "and says how to fix it: {warning}"
    );
}

#[tokio::test]
async fn an_http_target_is_vetted_before_it_is_ever_stored() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    // The cloud metadata endpoint is the classic: a worker POSTing each case there is an SSRF, not
    // a benchmark. Refused when the benchmark is written, not when the run reaches it.
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "b", "rubric": "x",
            "targets": [{
                "provider": "acme", "model": "rag",
                "kind": { "type": "http", "url": "https://169.254.169.254/latest/meta-data/" }
            }]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(err_message(&body).contains("refused"), "{body}");

    // A public https endpoint is stored, and comes back as the typed matrix it was written as.
    let stored = ok(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "b2", "rubric": "x",
            "targets": [{
                "provider": "acme", "model": "rag",
                "kind": { "type": "http", "url": "https://rag.acme.com/answer" }
            }]
        }),
    )
    .await;
    assert_eq!(
        stored["target"][0]["kind"]["url"],
        "https://rag.acme.com/answer"
    );
}

#[tokio::test]
async fn a_target_naming_a_prompt_this_project_does_not_have_is_refused() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    // A typo'd `prompt_ref` otherwise surfaces inside the run that was supposed to gate a deploy —
    // the worst moment and the least legible error.
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "b", "rubric": "x",
            "targets": [{
                "provider": "openai", "model": "gpt-4o",
                "prompt_ref": { "name": "suport-reply" }
            }]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        err_message(&body).contains("suport-reply"),
        "the refusal names the typo: {body}"
    );

    // A ref that pins both a version and a label is ambiguous, not merely wrong.
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "b", "rubric": "x",
            "targets": [{
                "provider": "openai", "model": "gpt-4o",
                "prompt_ref": { "name": "p", "version": 3, "label": "production" }
            }]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

/// `target` is documented free-form, and the product has taken two names out of it. A name a
/// caller may still write is not reserved: the value is read as policy whoever wrote it, and the
/// next name the product takes captures whatever callers were keeping under it. The door refuses
/// the taken names and the request field writes them, so the capability stays reachable.
#[tokio::test]
async fn the_reserved_keys_inside_a_free_form_target_are_the_hosts_to_write() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    // Failure mining, requested the way the product owns it.
    let stored = ok(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "mined", "rubric": "x",
            "target": { "endpoint": "https://rag.acme.com/answer" },
            "regression_dataset": "support-failures"
        }),
    )
    .await;
    assert_eq!(stored["target"]["regression_dataset"], "support-failures");
    assert_eq!(stored["target"]["endpoint"], "https://rag.acme.com/answer");

    // The same key, sent by the caller: refused, with the field to send instead named. Before this
    // door a caller's own note under that name silently became the dataset every failing verdict
    // in the project was appended to.
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "smuggled", "rubric": "x",
            "target": { "endpoint": "https://x", "regression_dataset": "my-own-note" }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(err_message(&body).contains("regression_dataset"), "{body}");

    // Recurrence is the older reservation and the more expensive capture: a number under that name
    // becomes a schedule row on the next boot, and paid runs follow.
    let (st, body) = send(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "smuggled-2", "rubric": "x",
            "target": { "endpoint": "https://x", "schedule_interval_secs": 60 }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        err_message(&body).contains("schedule_interval_secs"),
        "{body}"
    );

    // The floor: every other name in `target` is still the caller's, and a matrix target is
    // untouched by the check.
    let free = ok(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "free-form", "rubric": "x",
            "target": { "endpoint": "https://x", "regression_notes": "mine", "retries": 3 }
        }),
    )
    .await;
    assert_eq!(free["target"]["regression_notes"], "mine");
    assert_eq!(free["target"]["retries"], 3);
}
