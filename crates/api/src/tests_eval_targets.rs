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
async fn record_run(app: &axum::Router, bench: &str, pid: &str, extra: Value) {
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
            "started_at": "2026-01-01T00:00:00.000000000Z",
            "finished_at": "2026-01-01T00:05:00.000000000Z",
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
