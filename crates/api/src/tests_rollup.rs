//! `GET /v1/rollup` end to end: grouping, scoping, the admin-only dimension, and the 400s.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

async fn get(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ev(id: &str, model: &str, customer: &str) -> Value {
    json!({
        "id": id,
        "provider": "anthropic",
        "model": model,
        "usage": { "input": 100, "output": 50 },
        "metadata": { "customer_id": customer },
    })
}

#[tokio::test]
async fn a_project_key_rolls_up_its_own_traffic_by_any_dimension() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-r");
    let app = crate::build_router(state);

    for (id, model, customer) in [
        ("r-1", "claude-haiku-4-5", "acme"),
        ("r-2", "claude-haiku-4-5", "acme"),
        // A model absent from the test price book: unpriced, and the row must say so rather than
        // letting it read as $0.00 of spend.
        ("r-3", "model-with-no-price", "heavy"),
    ] {
        let (status, _) = ingest(&app, &key, ev(id, model, customer)).await;
        assert_eq!(status, StatusCode::OK, "{id}");
    }

    let (status, body) = get(&app, &key, "/v1/rollup?by=customer").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["group_by"], json!(["customer"]));
    assert_eq!(body["time_key"], "ts");
    let rows = body["rows"].as_array().expect("rows");
    let acme = rows
        .iter()
        .find(|r| r["keys"][0] == "acme")
        .expect("acme bucket");
    assert_eq!(acme["calls"], 2);
    assert_eq!(acme["input_tokens"], 200);
    assert_eq!(acme["unpriced_calls"], 0, "haiku is in the book");

    let heavy = rows
        .iter()
        .find(|r| r["keys"][0] == "heavy")
        .expect("heavy bucket");
    assert_eq!(
        heavy["unpriced_calls"], 1,
        "an unpriced call is disclosed, not summed as $0.00: {heavy}"
    );

    // A filter scopes the answer; another customer's spend never appears in it.
    let (status, body) = get(&app, &key, "/v1/rollup?by=model&filter=customer:acme").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "only acme's model: {rows:?}");
    assert_eq!(rows[0]["calls"], 2);
}

/// The one dimension that discloses something about the project's own credentials.
#[tokio::test]
async fn the_api_key_dimension_is_admin_only() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-r2");
    let app = crate::build_router(state);

    let (status, _) = get(&app, &key, "/v1/rollup?by=api_key").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get(&app, &key, "/v1/rollup?by=model&filter=api_key:whatever").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = get(&app, "admin-secret", "/v1/rollup?by=api_key").await;
    assert_eq!(status, StatusCode::OK, "an operator may ask");
}

/// A malformed request is a 400 with a reason, never a 500 and never a plausible-looking answer to
/// a different question.
#[tokio::test]
async fn malformed_requests_are_refused_with_a_reason() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-r3");
    let app = crate::build_router(state);

    for uri in [
        "/v1/rollup?by=nonsense",
        "/v1/rollup?by=model,model",
        "/v1/rollup?by=model,provider,name,day",
        "/v1/rollup?by=model&filter=no-colon",
        "/v1/rollup?by=model&filter=day:2026-01-01",
        "/v1/rollup?by=model&time=whenever",
        "/v1/rollup?by=model&since=not-a-date",
        "/v1/rollup?by=model&since=2026-06-02T00:00:00Z&until=2026-06-01T00:00:00Z",
    ] {
        let (status, _) = get(&app, &key, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

/// A project key's rollup is its own project's, whatever it asks for — the same scoping rule every
/// other read obeys, restated here because this route can group by `project`.
#[tokio::test]
async fn a_project_key_cannot_roll_up_another_project() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let key_b = make_key(&store, "proj-b");
    let app = crate::build_router(state);

    let (status, _) = ingest(&app, &key_b, ev("r-b1", "claude-haiku-4-5", "acme")).await;
    assert_eq!(status, StatusCode::OK);

    // Asking for someone else's project by name is refused outright, not silently re-scoped.
    let (status, _) = get(&app, &key_a, "/v1/rollup?by=project&project=proj-b").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // And asking for "everything" gets only proj-a — the `project` grouping cannot widen the scope.
    let (status, body) = get(&app, &key_a, "/v1/rollup?by=project").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["rows"].as_array().expect("rows");
    assert!(
        rows.iter().all(|r| r["keys"][0] != "proj-b"),
        "proj-b's traffic must not appear in proj-a's rollup: {rows:?}"
    );
}
