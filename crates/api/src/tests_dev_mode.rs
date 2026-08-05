//! Router-level tests for the dev-mode onboarding path — the zero-config first run.
//!
//! Two guarantees, and they pull in opposite directions, which is why they are pinned together:
//! a keyless dev-mode event that names no project must be *accepted* and attributed to a real
//! project (the documented quickstart used to 400 here, and the SDKs swallow the error, so the
//! user saw silence); and enforced mode must keep refusing exactly the same request, so the
//! convenience cannot leak into a deployment.
//!
//! Style follows `tests_ingest`: the wired `crate::build_router` over an in-memory `SqliteStore`,
//! driven through `tower`'s `oneshot`.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_store::Store;

use crate::auth::AuthMode;
use crate::guards::DEV_DEFAULT_PROJECT;
use crate::redact::Redactor;
use crate::tests_ingest::setup;

/// POST a body to `uri`, optionally with a bearer token. The `None` case is the one that matters
/// here: the quickstart sends no `Authorization` header at all.
async fn post(app: &Router, uri: &str, token: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

/// The event the README quickstart sends: no project, no key.
fn quickstart_event() -> Value {
    json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 }
    })
}

#[tokio::test]
async fn dev_mode_attributes_a_projectless_event_to_the_default_project() {
    let (mut state, store) = setup(Redactor::off());
    state.auth_mode = AuthMode::Dev;
    let app = crate::build_router(state);

    let (status, body) = post(&app, "/v1/events", None, quickstart_event()).await;

    // Accepted, and the response says where it went — the silent failure is gone from both sides.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], DEV_DEFAULT_PROJECT, "{body}");

    // Attributed for real: the row is queryable under that project, priced from the book like any
    // other event (1M-scale usage is not needed — this just proves the normal pipeline ran).
    let rows = store.list_events(Some(DEV_DEFAULT_PROJECT), 10).unwrap();
    assert_eq!(rows.len(), 1, "the event must be stored, not merely acknowledged");
    assert_eq!(rows[0].project_id, DEV_DEFAULT_PROJECT);
    assert!(rows[0].cost_usd.is_some(), "the dev default is a normal project: costing still runs");

    // And the project actually EXISTS — otherwise the events would live under an id that never
    // appears in GET /v1/projects, which is its own kind of silent.
    let proj = store.get_project(DEV_DEFAULT_PROJECT).unwrap();
    assert!(proj.is_some(), "the default project must be created, not just referenced");
    assert!(store.list_projects().unwrap().iter().any(|p| p.id == DEV_DEFAULT_PROJECT));

    // Repeatable: a second event reuses the row rather than erroring on the duplicate insert.
    let (s2, b2) = post(&app, "/v1/events", None, quickstart_event()).await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(store.list_events(Some(DEV_DEFAULT_PROJECT), 10).unwrap().len(), 2);
    assert_eq!(store.list_projects().unwrap().iter().filter(|p| p.id == DEV_DEFAULT_PROJECT).count(), 1);
}

#[tokio::test]
async fn dev_mode_default_applies_to_the_batch_door_too() {
    // A batching SDK must not be the one client that still fails silently.
    let (mut state, store) = setup(Redactor::off());
    state.auth_mode = AuthMode::Dev;
    let app = crate::build_router(state);

    let (status, body) =
        post(&app, "/v1/events/batch", None, json!([quickstart_event(), quickstart_event()])).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 2, "{body}");
    assert_eq!(body["invalid"], 0, "a projectless item is no longer invalid in dev mode: {body}");
    assert_eq!(store.list_events(Some(DEV_DEFAULT_PROJECT), 10).unwrap().len(), 2);
    assert!(store.get_project(DEV_DEFAULT_PROJECT).unwrap().is_some());
}

#[tokio::test]
async fn enforced_mode_still_refuses_an_unattributable_event() {
    // The production half of the same change. `setup` is already `AuthMode::Enforced`.
    let (state, store) = setup(Redactor::off());
    let app = crate::build_router(state);

    // Admin is the principal that exists in both modes — the one that could have leaked the dev
    // fallback into production. It must still get a 400.
    let (status, body) = post(&app, "/v1/events", Some("admin-secret"), quickstart_event()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request", "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("LIGHTTRACK_PROJECT"), "the 400 must name a fix: {body}");

    // No key at all: 401, unchanged — enforced mode never reaches project resolution.
    let (unauth, _) = post(&app, "/v1/events", None, quickstart_event()).await;
    assert_eq!(unauth, StatusCode::UNAUTHORIZED);

    // Nothing was written, and no default project was conjured up.
    assert!(store.list_events(Some(DEV_DEFAULT_PROJECT), 10).unwrap().is_empty());
    assert!(
        store.get_project(DEV_DEFAULT_PROJECT).unwrap().is_none(),
        "enforced mode must never create the dev default project"
    );

    // Same in a batch: the item is invalid, with the same actionable reason.
    let (bstatus, bbody) =
        post(&app, "/v1/events/batch", Some("admin-secret"), json!([quickstart_event()])).await;
    assert_eq!(bstatus, StatusCode::OK, "batch is multi-status under 200: {bbody}");
    assert_eq!(bbody["invalid"], 1, "{bbody}");
    assert_eq!(bbody["results"][0]["code"], "bad_request", "{bbody}");
    assert!(store.get_project(DEV_DEFAULT_PROJECT).unwrap().is_none());
}

#[tokio::test]
async fn a_project_key_is_unaffected_by_the_dev_default() {
    // Dev mode plus a real key: the key still forces its own project, and no stray `default`
    // project appears. The fallback is for the keyless path only.
    let (mut state, store) = setup(Redactor::off());
    state.auth_mode = AuthMode::Dev;
    let key = crate::tests_ingest::make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = post(&app, "/v1/events", Some(&key), quickstart_event()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], "proj-a", "{body}");
    assert!(store.get_project(DEV_DEFAULT_PROJECT).unwrap().is_none());
}
