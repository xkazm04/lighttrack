//! End-to-end over the wired router: the label ledger, judge trust, and the two things trust is
//! for — `require_trusted_judge` blocking a gate, and `needs_review` finding the verdicts a human
//! should look at.
//!
//! The unit tests in `judges` and `scores_review` pin the policies; these pin that the policies are
//! *reachable* — that a stored calibration really does turn a green benchmark gate into a 409 on a
//! project that asked for one, and that a human grade posted through the API really does surface the
//! verdict it contradicts.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::setup;

const ADMIN: &str = "admin-secret";

async fn send_as(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
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

async fn send(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send_as(app, ADMIN, method, uri, body).await
}

async fn ok(app: &axum::Router, method: &str, uri: &str, body: Value) -> Value {
    let (st, v) = send(app, method, uri, body).await;
    assert_eq!(st, StatusCode::OK, "{method} {uri}: {v}");
    v
}

fn app() -> axum::Router {
    let (state, _store) = setup(Redactor::off());
    crate::build_router(state)
}

async fn label(app: &axum::Router, subject: &str, value: f64) -> Value {
    ok(
        app,
        "POST",
        "/v1/labels",
        json!({
            "project_id": "p1",
            "subject": subject,
            "value": value,
            "labeler": "reviewer@example.com",
            "note": "graded in review",
        }),
    )
    .await
}

/// The floor: a human verdict survives the round trip with its attribution intact, and the ledger
/// can be narrowed to one subject. Before M11 none of this had anywhere to live.
#[tokio::test]
async fn a_label_round_trips_and_the_ledger_narrows_to_one_subject() {
    let app = app();
    let mine = label(&app, "event:ev1", 0.9).await;
    label(&app, "event:ev2", 0.1).await;
    assert_eq!(mine["labeler"], "reviewer@example.com");
    assert_eq!(mine["subject"]["type"], "event");
    assert_eq!(mine["subject"]["id"], "ev1");

    let page = ok(&app, "GET", "/v1/labels?project=p1", json!({})).await;
    assert_eq!(page["labels"].as_array().unwrap().len(), 2);

    let one = ok(
        &app,
        "GET",
        "/v1/labels?project=p1&subject=event:ev1",
        json!({}),
    )
    .await;
    let rows = one["labels"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{one}");
    assert_eq!(rows[0]["id"], mine["id"]);
}

/// A label with no attribution is refused, and a subject we cannot parse is a 400 rather than an
/// unnarrowed page — answering the whole ledger to a question about one event is worse than saying
/// the question was wrong.
#[tokio::test]
async fn an_unattributable_label_and_an_unparseable_subject_are_both_refused() {
    let app = app();
    let (st, _) = send(
        &app,
        "POST",
        "/v1/labels",
        json!({ "project_id": "p1", "subject": "event:e", "value": 0.5, "labeler": "  " }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "an unattributed label");

    let (st, _) = send(
        &app,
        "POST",
        "/v1/labels",
        json!({ "project_id": "p1", "subject": "trace:t1", "value": 0.5, "labeler": "me" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "an unknown subject kind");

    let (st, _) = send(&app, "GET", "/v1/labels?project=p1&subject=nope", json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "an unparseable subject filter");
}

/// Trust is three-valued and the absence of a measurement is `unknown` — never `untrusted`.
#[tokio::test]
async fn trust_is_unknown_until_a_calibration_says_otherwise() {
    let app = app();
    let v = ok(
        &app,
        "GET",
        "/v1/judges/trust?project=p1&rubric_id=rb1&judge=anthropic/haiku",
        json!({}),
    )
    .await;
    assert_eq!(v["trust"], "unknown");
    assert!(v.get("calibration").is_none(), "{v}");

    ok(
        &app,
        "POST",
        "/v1/calibrations",
        json!({
            "project_id": "p1", "judge": "anthropic/haiku", "rubric_id": "rb1",
            "kappa": 0.82, "pearson": 0.9, "mae": 0.05, "rmse": 0.07,
            "n": 40, "kappa_bar": 0.6, "trusted": true,
        }),
    )
    .await;

    let v = ok(
        &app,
        "GET",
        "/v1/judges/trust?project=p1&rubric_id=rb1&judge=anthropic/haiku",
        json!({}),
    )
    .await;
    assert_eq!(v["trust"], "trusted");
    assert_eq!(v["calibration"]["n"], 40, "the deciding record travels too");

    // …and it answers for THAT rubric only. A sibling rubric, and the freeform judge, are separate
    // facts — borrowing one for the other is the uncalibrated gate wearing a trusted badge.
    for uri in [
        "/v1/judges/trust?project=p1&rubric_id=rb2&judge=anthropic/haiku",
        "/v1/judges/trust?project=p1&judge=anthropic/haiku",
        "/v1/judges/trust?project=p1&rubric_id=rb1&judge=openai/gpt",
    ] {
        let v = ok(&app, "GET", uri, json!({})).await;
        assert_eq!(v["trust"], "unknown", "{uri} must not inherit trust: {v}");
    }
}

/// A rubric is `active` only once something has been measured against **its own id** — which is
/// what makes an M9 rubric version's unmeasured start visible instead of silent.
#[tokio::test]
async fn a_rubric_version_starts_inactive_and_does_not_inherit_its_predecessors_calibration() {
    let app = app();
    let rb = ok(
        &app,
        "POST",
        "/v1/projects/p1/rubrics",
        json!({ "name": "quality", "dimensions": [{ "key": "accuracy", "description": "is it right", "weight": 1.0 }] }),
    )
    .await;
    let id = rb["id"].as_str().unwrap().to_string();

    let view = ok(&app, "GET", &format!("/v1/rubrics/{id}"), json!({})).await;
    assert_eq!(view["active"], false, "nothing measured yet: {view}");

    ok(
        &app,
        "POST",
        "/v1/calibrations",
        json!({
            "project_id": "p1", "judge": "anthropic/haiku", "rubric_id": id,
            "kappa": 0.8, "pearson": 0.9, "mae": 0.05, "rmse": 0.07,
            "n": 30, "kappa_bar": 0.6, "trusted": true,
        }),
    )
    .await;
    let view = ok(&app, "GET", &format!("/v1/rubrics/{id}"), json!({})).await;
    assert_eq!(view["active"], true, "{view}");
    assert_eq!(view["calibrated_judges"][0], "anthropic/haiku");

    let v2 = ok(
        &app,
        "POST",
        &format!("/v1/rubrics/{id}/versions"),
        json!({ "threshold": 0.8 }),
    )
    .await;
    let v2id = v2["id"].as_str().unwrap();
    assert_ne!(v2id, id, "a version is a new row");
    let view = ok(&app, "GET", &format!("/v1/rubrics/{v2id}"), json!({})).await;
    assert_eq!(
        view["active"], false,
        "a new version inherits no calibration — promoting to it swaps a measured instrument for \
         an unmeasured one, and that must be visible: {view}"
    );
}

/// The gate consults trust, and a project that demands a trusted judge is refused (409) rather than
/// handed a green badge from an instrument nobody has checked.
#[tokio::test]
async fn require_trusted_judge_blocks_the_benchmark_gate_until_a_calibration_exists() {
    let app = app();
    ok(
        &app,
        "POST",
        "/v1/projects",
        json!({ "id": "p1", "name": "p1" }),
    )
    .await;
    let bench = ok(
        &app,
        "POST",
        "/v1/projects/p1/benchmarks",
        json!({
            "name": "support", "rubric": "be helpful", "rubric_id": "rb1",
            "judge_model": "anthropic/haiku", "baseline_score": 0.5,
        }),
    )
    .await;
    let bid = bench["id"].as_str().unwrap().to_string();

    // A green run, with the policy still off: the gate passes and *reports* the unknown trust.
    ok(
        &app,
        "POST",
        "/v1/benchmark-runs",
        json!({ "benchmark_id": bid, "status": "passed", "mean_score": 0.9,
                "n_cases": 10, "finished_at": "2026-01-01T00:00:00.000000000Z" }),
    )
    .await;
    let g = ok(
        &app,
        "GET",
        &format!("/v1/benchmarks/{bid}/gate"),
        json!({}),
    )
    .await;
    assert_eq!(g["status"], "pass");
    assert_eq!(
        g["judge_trust"]["trust"], "unknown",
        "a green badge from an unverified instrument must say so: {g}"
    );

    // Turn the policy on: the same evidence is now a refusal, and the refusal says which fix.
    ok(
        &app,
        "PUT",
        "/v1/projects/p1",
        json!({ "require_trusted_judge": true }),
    )
    .await;
    let (st, body) = send(
        &app,
        "GET",
        &format!("/v1/benchmarks/{bid}/gate"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("never been calibrated"), "{msg}");

    // Calibrate the exact (rubric, judge) pair the benchmark names, and the gate opens.
    ok(
        &app,
        "POST",
        "/v1/calibrations",
        json!({
            "project_id": "p1", "judge": "anthropic/haiku", "rubric_id": "rb1",
            "kappa": 0.9, "pearson": 0.95, "mae": 0.03, "rmse": 0.04,
            "n": 50, "kappa_bar": 0.6, "trusted": true,
        }),
    )
    .await;
    let g = ok(
        &app,
        "GET",
        &format!("/v1/benchmarks/{bid}/gate"),
        json!({}),
    )
    .await;
    assert_eq!(g["status"], "pass", "{g}");
    assert_eq!(g["judge_trust"]["trust"], "trusted");
}

/// `needs_review=1` finds the verdict a human contradicted — the signal that only exists once
/// labels and scores live in the same database.
#[tokio::test]
async fn needs_review_surfaces_the_verdict_a_human_disagreed_with() {
    let app = app();
    let good = ok(
        &app,
        "POST",
        "/v1/scores",
        json!({ "project_id": "p1", "rubric": "quality", "value": 0.95,
                "pass": true, "scored_by": "haiku" }),
    )
    .await;
    let disputed = ok(
        &app,
        "POST",
        "/v1/scores",
        json!({ "project_id": "p1", "rubric": "quality", "value": 0.92,
                "pass": true, "scored_by": "haiku" }),
    )
    .await;
    let disputed_id = disputed["id"].as_str().unwrap().to_string();

    // Everything is quiet until a human says otherwise.
    let page = ok(
        &app,
        "GET",
        "/v1/scores?project=p1&needs_review=1",
        json!({}),
    )
    .await;
    assert!(page.as_array().unwrap().is_empty(), "{page}");

    label(&app, &format!("score:{disputed_id}"), 0.2).await;
    let page = ok(
        &app,
        "GET",
        "/v1/scores?project=p1&needs_review=1",
        json!({}),
    )
    .await;
    let rows = page.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{page}");
    assert_eq!(rows[0]["id"], disputed_id);
    assert_ne!(rows[0]["id"], good["id"]);

    // A flag we do not recognise is a 400: "nothing to review" because of a typo is the worst
    // possible answer to a triage question.
    let (st, _) = send(
        &app,
        "GET",
        "/v1/scores?project=p1&needs_review=maybe",
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// Promote a graded production event into a golden set: the case is created with the event's text
/// and the human's grade is **copied onto it**, so the promoted case is not an input with no ground
/// truth — which is the state that made "golden sets" un-calibratable in the first place.
#[tokio::test]
async fn a_labelled_event_can_be_promoted_into_a_dataset_with_its_grade() {
    let app = app();
    ok(
        &app,
        "POST",
        "/v1/events",
        json!({ "id": "ev-promote", "project_id": "p1", "provider": "anthropic",
                "model": "claude-haiku-4-5", "input": "how do I reset my password?",
                "output": "click forgot password" }),
    )
    .await;
    let l = label(&app, "event:ev-promote", 0.95).await;
    let ds = ok(
        &app,
        "POST",
        "/v1/projects/p1/datasets",
        json!({ "name": "golden" }),
    )
    .await;
    let dsid = ds["id"].as_str().unwrap().to_string();

    let item = ok(
        &app,
        "POST",
        &format!("/v1/datasets/{dsid}/items/from-label"),
        json!({ "label_id": l["id"] }),
    )
    .await;
    assert_eq!(item["input"], "how do I reset my password?");
    assert_eq!(item["source_event_id"], "ev-promote");
    let item_id = item["id"].as_str().unwrap().to_string();

    let copied = ok(
        &app,
        "GET",
        &format!("/v1/labels?project=p1&subject=dataset_item:{item_id}"),
        json!({}),
    )
    .await;
    let rows = copied["labels"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the grade must travel with the case: {copied}"
    );
    assert_eq!(rows[0]["labeler"], "reviewer@example.com");
    assert_eq!(rows[0]["value"], 0.95);

    // The original event keeps its own grade — the label is copied, never moved.
    let orig = ok(
        &app,
        "GET",
        "/v1/labels?project=p1&subject=event:ev-promote",
        json!({}),
    )
    .await;
    assert_eq!(orig["labels"].as_array().unwrap().len(), 1, "{orig}");

    // A frozen set refuses the promotion rather than silently growing under a pinned run.
    ok(
        &app,
        "POST",
        &format!("/v1/datasets/{dsid}/freeze"),
        json!({}),
    )
    .await;
    let (st, _) = send(
        &app,
        "POST",
        &format!("/v1/datasets/{dsid}/items/from-label"),
        json!({ "label_id": l["id"] }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}
