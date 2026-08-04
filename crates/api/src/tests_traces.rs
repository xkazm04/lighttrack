//! End-to-end tests for the trace view + whole-trace scoring, over the wired axum router.
//!
//! They drive the same stack as ingest (`build_router` over an in-memory `SqliteStore`): ingest a
//! few events that share a `trace_id`, then read the rollup back through `GET /v1/traces` and
//! `/v1/traces/:id`, score the whole trace, and confirm tenant isolation on the read path.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

/// GET a path with a bearer token; return the status + parsed JSON body.
async fn get(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    send(app, token, "GET", path, None).await
}

async fn send(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

/// Ingest one event into a trace via the real router; return its persisted id.
async fn ingest_span(
    app: &Router,
    token: &str,
    trace: &str,
    span: &str,
    parent: Option<&str>,
    cost: f64,
) -> String {
    let mut body = json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 100, "output": 50 },
        "cost_usd": cost,
        "trace_id": trace,
        "span_id": span,
    });
    if let Some(p) = parent {
        body["parent_span_id"] = json!(p);
    }
    let (status, v) = ingest(app, token, body).await;
    assert_eq!(status, StatusCode::OK, "ingest failed: {v}");
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn trace_rollup_lists_aggregates_and_nests_spans() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // A three-span trace: root -> child -> grandchild.
    let root = ingest_span(&app, &key, "tr-1", "s1", None, 0.001).await;
    ingest_span(&app, &key, "tr-1", "s2", Some("s1"), 0.002).await;
    ingest_span(&app, &key, "tr-1", "s3", Some("s2"), 0.004).await;

    // List shows one rollup row with the summed cost + span count.
    let (status, list) = get(&app, &key, "/v1/traces").await;
    assert_eq!(status, StatusCode::OK);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1, "one trace: {list}");
    assert_eq!(rows[0]["trace_id"], "tr-1");
    assert_eq!(rows[0]["spans"], 3);
    assert!((rows[0]["cost_usd"].as_f64().unwrap() - 0.007).abs() < 1e-9, "{list}");

    // Detail nests the spans into a single chain and totals them.
    let (status, detail) = get(&app, &key, "/v1/traces/tr-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["totals"]["spans"], 3);
    assert_eq!(detail["status"], "success");
    let spans = detail["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 1, "single root");
    assert_eq!(spans[0]["event"]["id"], root, "root is the parentless span");
    assert_eq!(spans[0]["children"].as_array().unwrap().len(), 1);
    assert!(detail["scores"].as_array().unwrap().is_empty(), "no scores yet");

    // An unknown trace is 404.
    let (status, _) = get(&app, &key, "/v1/traces/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn score_whole_trace_anchors_to_root_and_surfaces_in_detail() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let root = ingest_span(&app, &key, "tr-1", "s1", None, 0.001).await;
    ingest_span(&app, &key, "tr-1", "s2", Some("s1"), 0.002).await;

    // Score the whole trace without naming an event: it anchors to the root span.
    let (status, score) = send(
        &app,
        &key,
        "POST",
        "/v1/traces/tr-1/score",
        Some(json!({
            "rubric": "trace-coherence",
            "value": 0.9,
            "pass": true,
            "scored_by": "claude-haiku-4-5"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{score}");
    assert_eq!(score["event_id"], root, "whole-trace score anchors to the root span");
    assert_eq!(score["project_id"], "proj-a");

    // It now shows up in the trace detail's scores.
    let (_, detail) = get(&app, &key, "/v1/traces/tr-1").await;
    let scores = detail["scores"].as_array().unwrap();
    assert_eq!(scores.len(), 1, "the whole-trace score joins back to the trace: {detail}");
    assert_eq!(scores[0]["rubric"], "trace-coherence");
}

#[tokio::test]
async fn project_key_cannot_read_another_projects_trace() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let key_b = make_key(&store, "proj-b");
    let app = crate::build_router(state);

    ingest_span(&app, &key_a, "tr-a", "s1", None, 0.001).await;

    // B's key may not read A's trace. It reads as 404, not 403: the trace read is scoped by project
    // in the query, so A's trace simply does not exist for B (no existence oracle either).
    let (status, body) = get(&app, &key_b, "/v1/traces/tr-a").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // And B's listing doesn't include A's trace.
    let (status, list) = get(&app, &key_b, "/v1/traces").await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.as_array().unwrap().is_empty(), "cross-tenant trace leaked: {list}");
}

/// The collision case: two projects legitimately reuse the same natural trace id (a shared upstream
/// request id). Before the read was project-scoped, the merged trace's owner was decided by the
/// *earliest* event, so whichever tenant posted first (or deliberately backdated) owned — and could
/// read — the other's spans, inputs and outputs included.
#[tokio::test]
async fn colliding_trace_id_never_merges_across_projects() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let key_b = make_key(&store, "proj-b");
    let app = crate::build_router(state);

    // A ingests first (so A's span is the oldest — the one that used to claim the merged trace).
    let a_root = ingest_span(&app, &key_a, "req-1", "s-a", None, 0.001).await;
    let b_root = ingest_span(&app, &key_b, "req-1", "s-b", None, 0.050).await;

    // Detail: each side sees exactly its own single span, and its own project/cost.
    let (status, a_detail) = get(&app, &key_a, "/v1/traces/req-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(a_detail["project_id"], "proj-a");
    assert_eq!(a_detail["totals"]["spans"], 1, "B's span leaked into A's trace: {a_detail}");
    assert_eq!(a_detail["spans"].as_array().unwrap()[0]["event"]["id"], a_root);
    assert!((a_detail["totals"]["cost_usd"].as_f64().unwrap() - 0.001).abs() < 1e-9);

    let (status, b_detail) = get(&app, &key_b, "/v1/traces/req-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(b_detail["project_id"], "proj-b", "the older foreign span must not claim the trace");
    assert_eq!(b_detail["totals"]["spans"], 1, "A's span leaked into B's trace: {b_detail}");
    assert_eq!(b_detail["spans"].as_array().unwrap()[0]["event"]["id"], b_root);

    // List: each side's rollup counts only its own spans.
    for (key, cost) in [(&key_a, 0.001), (&key_b, 0.050)] {
        let (_, list) = get(&app, key, "/v1/traces").await;
        let rows = list.as_array().unwrap();
        assert_eq!(rows.len(), 1, "{list}");
        assert_eq!(rows[0]["spans"], 1, "cross-project spans merged into the listing: {list}");
        assert!((rows[0]["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-9, "{list}");
    }

    // Whole-trace scoring anchors to the caller's own root, never the foreign one, and each side's
    // detail shows only its own verdicts.
    let (status, score) = send(
        &app,
        &key_b,
        "POST",
        "/v1/traces/req-1/score",
        Some(json!({ "rubric": "trace-coherence", "value": 0.5, "scored_by": "judge" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{score}");
    assert_eq!(score["project_id"], "proj-b");
    assert_eq!(score["event_id"], b_root, "score anchored to the foreign project's root span");

    let (_, a_detail) = get(&app, &key_a, "/v1/traces/req-1").await;
    assert!(
        a_detail["scores"].as_array().unwrap().is_empty(),
        "B's verdict surfaced in A's trace: {a_detail}"
    );
    let (_, b_detail) = get(&app, &key_b, "/v1/traces/req-1").await;
    assert_eq!(b_detail["scores"].as_array().unwrap().len(), 1);
}
