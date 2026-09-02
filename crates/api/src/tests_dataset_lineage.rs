//! End-to-end over the wired router: forking a dataset, mining rows into it, and walking the
//! version history (M24).
//!
//! The unit-level semantics are pinned by the store conformance suite. What these pin is that the
//! loop is *reachable through HTTP*: a frozen golden set can be extended by forking rather than by
//! building a differently-named one, a failing production call can become a permanent eval case in
//! one POST, and appending to a frozen corpus is a 409 rather than a silent write.

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

fn app() -> axum::Router {
    let (state, _store) = setup(Redactor::off());
    crate::build_router(state)
}

/// The whole point of the milestone in one test: a frozen set stops being a dead end.
#[tokio::test]
async fn a_frozen_dataset_is_extended_by_forking_it_not_by_writing_to_it() {
    let app = app();
    let ds = ok(
        &app,
        "POST",
        "/v1/projects/p1/datasets",
        json!({ "name": "golden", "source": "manual" }),
    )
    .await;
    let id = ds["id"].as_str().expect("id").to_string();
    assert_eq!(ds["version"], 1);
    assert_eq!(ds["parent_id"], Value::Null, "a created set has no parent");

    ok(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/items"),
        json!({ "input": "what is 2+2", "expected": "4" }),
    )
    .await;
    ok(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/freeze"),
        json!({}),
    )
    .await;

    // The pre-M24 answer to "add a case to a frozen set" — still a 409, and now with a way forward.
    let (st, _) = send(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/items"),
        json!({ "input": "another" }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, body) = send(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/items/import"),
        json!({ "n": 5 }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CONFLICT,
        "importing into a frozen corpus rewrites what a finished run was scored against: {body}"
    );

    let v2 = ok(&app, "POST", &format!("/v1/datasets/{id}/fork"), json!({})).await;
    assert_eq!(v2["version"], 2, "the fork is the next version");
    assert_eq!(v2["parent_id"], json!(id), "and it is linked to its parent");
    assert_eq!(v2["frozen"], json!(false));
    assert_eq!(v2["name"], json!("golden"), "same name — that is the key");

    let v2id = v2["id"].as_str().expect("id").to_string();
    let items = ok(
        &app,
        "GET",
        &format!("/v1/datasets/{v2id}/items"),
        json!({}),
    )
    .await;
    assert_eq!(items.as_array().expect("items").len(), 1, "cases copied");

    // …and writing to the fork works, which is the whole reason it exists.
    ok(
        &app,
        "POST",
        &format!("/v1/datasets/{v2id}/items"),
        json!({ "input": "what is 3+3", "expected": "6" }),
    )
    .await;

    let versions = ok(
        &app,
        "GET",
        "/v1/projects/p1/datasets/versions?name=golden",
        json!({}),
    )
    .await;
    let vs = versions.as_array().expect("versions");
    assert_eq!(vs.len(), 2);
    assert_eq!(vs[0]["version"], 2, "newest version first");
    assert_eq!(vs[1]["version"], 1);
}

/// The failure-mining path: a real call becomes a permanent eval case, scrubbed, with its
/// provenance and its fingerprint attached.
#[tokio::test]
async fn a_production_event_can_be_mined_into_a_dataset_and_is_scrubbed_on_the_way_in() {
    let app = app();
    let ev = ok(
        &app,
        "POST",
        "/v1/events",
        json!({
            "project_id": "p1",
            "provider": "anthropic",
            "model": "m1",
            "status": "error",
            "input": "refund the order for bob@example.com",
            "output": "sorry",
        }),
    )
    .await;
    let evid = ev["id"].as_str().expect("event id").to_string();

    let ds = ok(
        &app,
        "POST",
        "/v1/projects/p1/datasets",
        json!({ "name": "regressions" }),
    )
    .await;
    let id = ds["id"].as_str().expect("id").to_string();

    let out = ok(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/items/import"),
        json!({ "from": "events", "strategy": "errors", "n": 10, "event_ids": [evid] }),
    )
    .await;
    assert_eq!(out["imported"], 1);
    assert_eq!(out["dataset_id"], json!(id));

    let items = ok(&app, "GET", &format!("/v1/datasets/{id}/items"), json!({})).await;
    let item = &items.as_array().expect("items")[0];
    assert_eq!(item["source_event_id"], json!(evid), "provenance is kept");
    assert!(
        !item["input"]
            .as_str()
            .expect("input")
            .contains("bob@example.com"),
        "mined production text must be scrubbed before it becomes a stored case: {item}"
    );
    assert_eq!(item["anonymization"]["method"], "regex");
    assert!(
        item["input_hash"].is_string(),
        "dedupe needs the fingerprint"
    );

    // Re-importing the same event with dedupe on is a no-op, not a second copy — the property a
    // regression-mining loop running every scoring cycle depends on.
    let again = ok(
        &app,
        "POST",
        &format!("/v1/datasets/{id}/items/import"),
        json!({ "dedupe": true, "event_ids": [evid] }),
    )
    .await;
    assert_eq!(again["imported"], 0);
    let items = ok(&app, "GET", &format!("/v1/datasets/{id}/items"), json!({})).await;
    assert_eq!(items.as_array().expect("items").len(), 1);
}

/// The version walk is a project read, and a name nobody has used is empty rather than an error.
#[tokio::test]
async fn the_version_walk_is_scoped_and_empty_for_an_unknown_name() {
    let app = app();
    let v = ok(
        &app,
        "GET",
        "/v1/projects/p1/datasets/versions?name=never-created",
        json!({}),
    )
    .await;
    assert!(v.as_array().expect("versions").is_empty());

    let (st, _) = send(&app, "GET", "/v1/projects/p1/datasets/versions", json!({})).await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "the name is what is being walked; without it there is nothing to answer"
    );
}
