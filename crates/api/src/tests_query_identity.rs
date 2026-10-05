//! Identity in **query position**: the read doors must resolve a trace id and a provider id
//! through the same canonicalizers the ingest doors apply, so an operator pasting a W3C trace id
//! as the spec prints it (uppercase hex) does not get a 404 on a trace that exists.
//!
//! The dimensions that must stay EXACT are pinned here too: `?model=` is the pricing/limits merge
//! key and is deliberately not folded, and an unmodeled provider stays queryable rather than
//! becoming a validation error.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

/// The W3C spec's own example trace id, in the spec's own case — the exact fixture
/// `crates/core/src/trace.rs` normalizes on the write path.
const W3C_UPPER: &str = "9F2C4D6E8A0B1C3D5E7F9A1B2C3D4E5F";
const W3C_LOWER: &str = "9f2c4d6e8a0b1c3d5e7f9a1b2c3d4e5f";

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
            .expect("request"),
        None => req.body(Body::empty()).expect("request"),
    };
    let resp = app.clone().oneshot(req).await.expect("response");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

async fn get(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    send(app, token, "GET", path, None).await
}

/// Ingest one span through the real SDK door.
async fn ingest_span(app: &Router, token: &str, trace: &str, span: &str, provider: &str) {
    let (status, v) = ingest(
        app,
        token,
        json!({
            "provider": provider,
            "model": "claude-haiku-4-5",
            "usage": { "input": 100, "output": 50 },
            "trace_id": trace,
            "span_id": span,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ingest failed: {v}");
}

/// Case 1: the detail door resolves the uppercase spelling of a stored (lower-cased) W3C id.
#[tokio::test]
async fn the_detail_door_resolves_an_uppercase_w3c_trace_id() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    ingest_span(&app, &key, W3C_UPPER, "aaaabbbbccccdddd", "anthropic").await;

    let (status, body) = get(&app, &key, &format!("/v1/traces/{W3C_UPPER}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a W3C id in the spec's own case must resolve: {body}"
    );
    assert_eq!(body["trace_id"], W3C_LOWER);
}

/// Case 2: so does the events filter.
#[tokio::test]
async fn the_events_filter_resolves_an_uppercase_w3c_trace_id() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    ingest_span(&app, &key, W3C_UPPER, "aaaabbbbccccdddd", "anthropic").await;
    ingest_span(&app, &key, W3C_UPPER, "1111222233334444", "anthropic").await;

    let (status, body) = get(&app, &key, &format!("/v1/events?trace_id={W3C_UPPER}")).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("events array");
    assert_eq!(
        rows.len(),
        2,
        "an empty page here is indistinguishable from 'that trace has no events': {body}"
    );
}

/// Case 4: `?provider=` canonicalizes exactly as `ProviderId` does on the way in.
#[tokio::test]
async fn the_provider_filter_canonicalizes_like_the_ingest_door() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    ingest_span(&app, &key, "trace-p1", "span-p1", "openai").await;
    ingest_span(&app, &key, "trace-p2", "span-p2", "Azure OpenAI").await;

    let (status, body) = get(&app, &key, "/v1/events?provider=OpenAI").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("events array");
    assert_eq!(rows.len(), 1, "?provider=OpenAI must find `openai`: {body}");
    assert_eq!(rows[0]["provider"], "openai");

    let (status, body) = get(&app, &key, "/v1/events?provider=Azure%20OpenAI").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("events array");
    assert_eq!(
        rows.len(),
        1,
        "?provider=Azure OpenAI must find `azure-openai`: {body}"
    );
    assert_eq!(rows[0]["provider"], "azure-openai");
}

/// Case 5: the rollup's `filter=provider:` buckets identically whichever spelling is asked.
#[tokio::test]
async fn the_rollup_provider_filter_canonicalizes_too() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    ingest_span(&app, &key, "trace-r1", "span-r1", "openai").await;
    ingest_span(&app, &key, "trace-r2", "span-r2", "anthropic").await;

    let (status, lower) = get(&app, &key, "/v1/rollup?by=provider&filter=provider:openai").await;
    assert_eq!(status, StatusCode::OK);
    let (status, upper) = get(&app, &key, "/v1/rollup?by=provider&filter=provider:OpenAI").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        upper["rows"], lower["rows"],
        "the same bucket, whichever case the operator pasted"
    );
    assert_eq!(lower["rows"].as_array().expect("rows").len(), 1);
}

/// GUARD (case 6): `?model=` stays an EXACT match. Model identity is the pricing/limits merge key —
/// folding it here would conflate two different questions.
#[tokio::test]
async fn guard_the_model_filter_stays_exact() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    for (span, model) in [("span-m1", "gpt-4o"), ("span-m2", "gpt-4o-2024-08-06")] {
        let (status, v) = ingest(
            &app,
            &key,
            json!({
                "provider": "openai",
                "model": model,
                "usage": { "input": 10, "output": 5 },
                "trace_id": "trace-m",
                "span_id": span,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "ingest failed: {v}");
    }

    let (status, body) = get(&app, &key, "/v1/events?model=gpt-4o").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("events array");
    assert_eq!(
        rows.len(),
        1,
        "?model=gpt-4o must not start matching gpt-4o-2024-08-06: {body}"
    );
    assert_eq!(rows[0]["model"], "gpt-4o");

    // And case is still meaningful for a model id.
    let (status, body) = get(&app, &key, "/v1/events?model=GPT-4o").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array().expect("events array").is_empty(),
        "model matching is exact, not case-folded: {body}"
    );
}

/// Case 7: the whole-trace score door anchors the uppercase spelling to the same trace the
/// lowercase spelling resolves to — one verdict row, visible from both.
#[tokio::test]
async fn the_score_door_anchors_an_uppercase_trace_id_to_the_same_trace() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    ingest_span(&app, &key, W3C_UPPER, "aaaabbbbccccdddd", "anthropic").await;

    let (status, body) = send(
        &app,
        &key,
        "POST",
        &format!("/v1/traces/{W3C_UPPER}/score"),
        Some(json!({
            "rubric": "helpfulness",
            "value": 0.75,
            "scored_by": "tests",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "uppercase score POST: {body}");

    for spelling in [W3C_UPPER, W3C_LOWER] {
        let (status, detail) = get(&app, &key, &format!("/v1/traces/{spelling}")).await;
        assert_eq!(status, StatusCode::OK, "{spelling}: {detail}");
        let scores = detail["scores"].as_array().expect("scores");
        assert_eq!(
            scores.len(),
            1,
            "exactly one verdict, visible from {spelling}: {detail}"
        );
        assert_eq!(scores[0]["rubric"], "helpfulness");
    }
}

/// GUARD (case 8): an unmodeled provider is accepted on ingest, so it must stay *queryable* —
/// an empty page, never a 400.
#[tokio::test]
async fn guard_an_unmodeled_provider_filter_answers_an_empty_page() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-qi");
    let app = crate::build_router(state);
    ingest_span(&app, &key, "trace-u", "span-u", "anthropic").await;

    let (status, body) = get(&app, &key, "/v1/events?provider=totally-unheard-of").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an unmodeled provider is a filter, not a validation error: {body}"
    );
    assert!(body.as_array().expect("events array").is_empty());
}
