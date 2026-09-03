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
    assert!(
        (rows[0]["cost_usd"].as_f64().unwrap() - 0.007).abs() < 1e-9,
        "{list}"
    );

    // Detail nests the spans into a single chain and totals them.
    let (status, detail) = get(&app, &key, "/v1/traces/tr-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["totals"]["spans"], 3);
    assert_eq!(detail["status"], "success");
    let spans = detail["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 1, "single root");
    assert_eq!(spans[0]["event"]["id"], root, "root is the parentless span");
    assert_eq!(spans[0]["children"].as_array().unwrap().len(), 1);
    assert!(
        detail["scores"].as_array().unwrap().is_empty(),
        "no scores yet"
    );

    // An unknown trace is 404.
    let (status, _) = get(&app, &key, "/v1/traces/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// One definition of a trace: the list rollup and the detail view must report the same duration and
/// status. They drifted because the list computed `MAX(ts) - MIN(ts)` (start-to-start) while the
/// detail counted the final span's latency — so this pins the exact case, a trailing span with
/// non-trivial latency.
#[tokio::test]
async fn list_and_detail_agree_on_duration_and_status() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let span = |ts: &str, id: &str, latency: u64, status: &str| {
        json!({
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 0.001,
            "trace_id": "tr-dur", "span_id": id, "ts": ts,
            "latency_ms": latency, "status": status
        })
    };
    let now = chrono::Utc::now();
    let t0 = (now - chrono::Duration::seconds(30)).to_rfc3339();
    let t2 = (now - chrono::Duration::seconds(28)).to_rfc3339();
    for body in [
        span(&t0, "s1", 120, "success"),
        span(&t2, "s2", 750, "error"),
    ] {
        let (status, v) = ingest(&app, &key, body).await;
        assert_eq!(status, StatusCode::OK, "{v}");
    }

    let (_, list) = get(&app, &key, "/v1/traces").await;
    let row = &list.as_array().unwrap()[0];
    let (_, detail) = get(&app, &key, "/v1/traces/tr-dur").await;

    assert_eq!(
        row["duration_ms"], 2750,
        "the trailing span's latency counts in the list: {list}"
    );
    assert_eq!(
        row["duration_ms"], detail["duration_ms"],
        "list vs detail duration"
    );
    assert_eq!(row["status"], detail["status"], "list vs detail status");
    assert_eq!(row["status"], "error");
    // A complete trace still says so explicitly.
    assert_eq!(detail["spans_truncated"], false);
    assert_eq!(detail["spans_total"], 2);
    assert_eq!(detail["spans_logged"], 2);
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
    assert_eq!(
        score["event_id"], root,
        "whole-trace score anchors to the root span"
    );
    assert_eq!(score["project_id"], "proj-a");

    // It now shows up in the trace detail's scores.
    let (_, detail) = get(&app, &key, "/v1/traces/tr-1").await;
    let scores = detail["scores"].as_array().unwrap();
    assert_eq!(
        scores.len(),
        1,
        "the whole-trace score joins back to the trace: {detail}"
    );
    assert_eq!(scores[0]["rubric"], "trace-coherence");
}

/// A whole-trace verdict records what it judged, and a trace that moved underneath it says so on
/// read. The two drifts are deliberately different: extra spans make the verdict *narrower* than the
/// trace (stale, but re-judging would re-send identical text), while a changed root exchange means
/// the verdict describes text that is no longer there.
#[tokio::test]
async fn a_verdict_records_its_coverage_and_a_changed_trace_reads_stale() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let exchange = |span: &str, output: &str| {
        json!({
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 0.001,
            "trace_id": "tr-cov", "span_id": span,
            "input": "what is 2+2?", "output": output,
        })
    };
    let (status, root) = ingest(&app, &key, exchange("s1", "4")).await;
    assert_eq!(status, StatusCode::OK, "{root}");
    let root_id = root["id"].as_str().unwrap().to_string();

    let score = |app: &Router, key: &str| {
        let body = json!({ "rubric": "trace-coherence", "value": 0.9, "pass": true,
                           "scored_by": "claude-haiku-4-5" });
        let (app, key) = (app.clone(), key.to_string());
        async move { send(&app, &key, "POST", "/v1/traces/tr-cov/score", Some(body)).await }
    };
    let (status, posted) = score(&app, &key).await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    let cov = &posted["detail"]["coverage"];
    assert_eq!(
        cov["spans"], 1,
        "the verdict records the trace it judged: {posted}"
    );
    assert_eq!(cov["root_event_id"], root_id);
    let digest = cov["digest"].as_str().unwrap().to_string();
    assert_eq!(
        digest.len(),
        16,
        "content fingerprint of the judged exchange: {posted}"
    );

    // Nothing has moved yet: the verdict covers the trace, so nothing is reported.
    let (_, detail) = get(&app, &key, "/v1/traces/tr-cov").await;
    assert!(
        detail["scores"][0]["stale"].is_null(),
        "a current verdict is not stale: {detail}"
    );

    // A late child span lands. The trace is wider than the verdict, but the judged root exchange is
    // byte-identical.
    ingest_span(&app, &key, "tr-cov", "s2", Some("s1"), 0.002).await;
    let (_, detail) = get(&app, &key, "/v1/traces/tr-cov").await;
    assert_eq!(
        detail["totals"]["spans"], 2,
        "the late span is folded into the read: {detail}"
    );
    assert_eq!(detail["scores"][0]["stale"]["reason"], "grown", "{detail}");
    assert_eq!(detail["scores"][0]["stale"]["scored_spans"], 1);
    assert_eq!(detail["scores"][0]["stale"]["current_spans"], 2);

    // Now the judged exchange itself moves: a *new* root lands ahead of the scored one (an OTel
    // parent span exported when it finished). The verdict describes text that is no longer judged.
    let mut late_root = exchange("s0", "the real answer is 4, with working");
    late_root["ts"] = json!((chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339());
    let (status, v) = ingest(&app, &key, late_root).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, detail) = get(&app, &key, "/v1/traces/tr-cov").await;
    assert_eq!(
        detail["scores"][0]["stale"]["reason"], "changed",
        "{detail}"
    );

    // Re-scoring the trace writes a verdict whose coverage matches the trace as it now reads, and
    // that one is not stale — so a corrected trace does not stay flagged forever.
    let (status, posted) = score(&app, &key).await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_ne!(
        posted["detail"]["coverage"]["digest"],
        json!(digest),
        "a new exchange, a new digest"
    );
    let (_, detail) = get(&app, &key, "/v1/traces/tr-cov").await;
    let scores = detail["scores"].as_array().unwrap();
    assert_eq!(scores.len(), 2, "{detail}");
    assert!(
        scores.iter().any(|s| s["stale"].is_null()),
        "the corrected verdict covers the trace: {detail}"
    );
}

/// A per-call verdict pinned to an inner span is not a whole-trace judgment, so it carries no
/// whole-trace coverage — and never reads as stale when the trace grows around it.
#[tokio::test]
async fn a_per_call_verdict_carries_no_whole_trace_coverage() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    ingest_span(&app, &key, "tr-call", "s1", None, 0.001).await;
    let child = ingest_span(&app, &key, "tr-call", "s2", Some("s1"), 0.002).await;

    let (status, posted) = send(
        &app,
        &key,
        "POST",
        "/v1/traces/tr-call/score",
        Some(
            json!({ "rubric": "faithfulness", "value": 0.5, "scored_by": "judge",
                     "event_id": child }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert!(
        posted["detail"]["coverage"].is_null(),
        "a per-call score gets no trace coverage: {posted}"
    );

    ingest_span(&app, &key, "tr-call", "s3", Some("s1"), 0.004).await;
    let (_, detail) = get(&app, &key, "/v1/traces/tr-call").await;
    assert!(
        detail["scores"][0]["stale"].is_null(),
        "not a whole-trace verdict: {detail}"
    );
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
    assert!(
        list.as_array().unwrap().is_empty(),
        "cross-tenant trace leaked: {list}"
    );
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
    assert_eq!(
        a_detail["totals"]["spans"], 1,
        "B's span leaked into A's trace: {a_detail}"
    );
    assert_eq!(
        a_detail["spans"].as_array().unwrap()[0]["event"]["id"],
        a_root
    );
    assert!((a_detail["totals"]["cost_usd"].as_f64().unwrap() - 0.001).abs() < 1e-9);

    let (status, b_detail) = get(&app, &key_b, "/v1/traces/req-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        b_detail["project_id"], "proj-b",
        "the older foreign span must not claim the trace"
    );
    assert_eq!(
        b_detail["totals"]["spans"], 1,
        "A's span leaked into B's trace: {b_detail}"
    );
    assert_eq!(
        b_detail["spans"].as_array().unwrap()[0]["event"]["id"],
        b_root
    );

    // List: each side's rollup counts only its own spans.
    for (key, cost) in [(&key_a, 0.001), (&key_b, 0.050)] {
        let (_, list) = get(&app, key, "/v1/traces").await;
        let rows = list.as_array().unwrap();
        assert_eq!(rows.len(), 1, "{list}");
        assert_eq!(
            rows[0]["spans"], 1,
            "cross-project spans merged into the listing: {list}"
        );
        assert!(
            (rows[0]["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-9,
            "{list}"
        );
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
    assert_eq!(
        score["event_id"], b_root,
        "score anchored to the foreign project's root span"
    );

    let (_, a_detail) = get(&app, &key_a, "/v1/traces/req-1").await;
    assert!(
        a_detail["scores"].as_array().unwrap().is_empty(),
        "B's verdict surfaced in A's trace: {a_detail}"
    );
    let (_, b_detail) = get(&app, &key_b, "/v1/traces/req-1").await;
    assert_eq!(b_detail["scores"].as_array().unwrap().len(), 1);
}

/// The score door used to look only at the trace's root-level spans: a per-call verdict on a nested
/// call never found its event, a whole-trace verdict counted redacted evidence on the roots only,
/// and an `event_id` from anywhere at all was stored unchecked.
#[tokio::test]
async fn the_score_door_sees_every_span_and_refuses_a_foreign_anchor() {
    let (state, store) = setup(Redactor::all());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // A three-deep chain, every span carrying an email the scrubber rewrites.
    let mut ids = Vec::new();
    for (span, parent) in [("s1", None), ("s2", Some("s1")), ("s3", Some("s2"))] {
        let mut body = json!({
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 0.001,
            "trace_id": "tr-deep", "span_id": span,
            "input": "mail ada@example.com", "output": "sent",
        });
        if let Some(p) = parent {
            body["parent_span_id"] = json!(p);
        }
        let (status, v) = ingest(&app, &key, body).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        ids.push(v["id"].as_str().unwrap().to_string());
    }

    // A whole-trace verdict counts the evidence of all three spans, not the root alone.
    let (status, posted) = send(
        &app,
        &key,
        "POST",
        "/v1/traces/tr-deep/score",
        Some(json!({ "rubric": "coherence", "value": 0.9, "scored_by": "judge" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_eq!(
        posted["detail"]["evidence_redacted_spans"], 3,
        "one scrubbed span per event, three events: {posted}"
    );

    // A per-call verdict on the grandchild is accepted and carries that span's own evidence.
    let (status, posted) = send(
        &app,
        &key,
        "POST",
        "/v1/traces/tr-deep/score",
        Some(
            json!({ "rubric": "faithfulness", "value": 0.5, "scored_by": "judge",
                     "event_id": ids[2] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_eq!(posted["event_id"], ids[2].as_str());
    assert_eq!(posted["detail"]["evidence_redacted_spans"], 1, "{posted}");

    // An anchor that is not a span of this trace is refused, not stored.
    let (status, refused) = send(
        &app,
        &key,
        "POST",
        "/v1/traces/tr-deep/score",
        Some(json!({ "rubric": "x", "value": 0.5, "scored_by": "judge",
                     "event_id": "ev-from-somewhere-else" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

/// The listing refuses the same malformed windows `/v1/events` refuses, instead of paging back an
/// empty array that reads as "no traces".
#[tokio::test]
async fn an_inverted_window_or_a_nan_cost_floor_is_a_400_not_an_empty_page() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let (status, body) = get(
        &app,
        &key,
        "/v1/traces?since=2026-02-01T00:00:00Z&until=2026-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = get(&app, &key, "/v1/traces?min_cost=NaN").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = get(&app, &key, "/v1/traces?min_cost=0.5").await;
    assert_eq!(status, StatusCode::OK);
}

/// The whole-trace door validates the verdict's numbers with the same rule as `POST /v1/scores`.
#[tokio::test]
async fn a_trace_verdict_that_is_not_a_score_is_refused() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    ingest_span(&app, &key, "tr-v", "s1", None, 0.001).await;
    for body in [
        json!({ "rubric": "q", "value": 1.5, "max": 1.0, "scored_by": "judge" }),
        json!({ "rubric": "q", "value": 0.5, "max": 0.0, "scored_by": "judge" }),
        json!({ "rubric": "q", "value": 0.5, "scored_by": "" }),
    ] {
        let (status, v) = send(
            &app,
            &key,
            "POST",
            "/v1/traces/tr-v/score",
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {v}");
    }
}
