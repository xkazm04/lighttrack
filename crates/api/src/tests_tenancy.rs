//! End-to-end tenancy-lifecycle tests: the project kill switch on every ingest door, key scoping,
//! key expiry, rotation with a grace window, and archival.
//!
//! These drive the wired router (`crate::build_router`) over an in-memory store, because every
//! guarantee here is a property of the *stack* — a scope decided in `auth_scopes` is only real if
//! the handler's guard actually consults it.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{new_id, ApiKey, Scope};
use lighttrack_store::{SqliteStore, Store};

use crate::auth;
use crate::redact::Redactor;
use crate::state::AppState;
use crate::tests_ingest::{ingest, make_key, setup};
use lighttrack_store::Scope as TenantScope;

const ADMIN: &str = "admin-secret";

/// Mint a key on an existing project with explicit scopes and an optional expiry.
fn key_with(
    store: &SqliteStore,
    project_id: &str,
    scopes: Vec<Scope>,
    expires_at: Option<chrono::DateTime<Utc>>,
) -> String {
    let g = auth::generate_key();
    store
        .create_api_key(&ApiKey {
            id: new_id(),
            project_id: project_id.into(),
            name: "scoped".into(),
            prefix: g.prefix.clone(),
            key_hash: g.key_hash,
            created_at: Utc::now(),
            last_used_at: None,
            revoked: false,
            scopes,
            expires_at,
        })
        .unwrap();
    g.full_key
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

async fn get(app: &Router, uri: &str, token: &str) -> (StatusCode, Value) {
    send(app, "GET", uri, token, Value::Null).await
}

fn event() -> Value {
    json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 1, "output": 1 }
    })
}

/// A minimal but valid OTLP export, so the third ingest door can be exercised without dragging in
/// the full semconv fixture.
fn otlp_export() -> Value {
    let now = Utc::now().timestamp_nanos_opt().unwrap();
    json!({
      "resourceSpans": [{
        "scopeSpans": [{
          "spans": [{
            "traceId": "5b8efff798038103d269b633813fc60c",
            "spanId": "eee19b7ec3c1b174",
            "name": "chat claude-haiku-4-5",
            "startTimeUnixNano": (now - 1_000_000_000).to_string(),
            "endTimeUnixNano": now.to_string(),
            "attributes": [
              { "key": "gen_ai.system", "value": { "stringValue": "anthropic" } },
              { "key": "gen_ai.request.model", "value": { "stringValue": "claude-haiku-4-5" } },
              { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "10" } },
              { "key": "gen_ai.usage.output_tokens", "value": { "intValue": "5" } }
            ]
          }]
        }]
      }]
    })
}

fn disable(state: &AppState, store: &Arc<SqliteStore>, pid: &str) {
    let mut p = store.get_project(pid).unwrap().unwrap();
    p.enabled = false;
    assert!(store.update_project(&p).unwrap());
    state.project_policies.invalidate(pid);
}

/// The headline regression M16 closes: `enabled = false` used to be read by exactly one non-auth
/// caller, so "disable this project" stopped nothing. Every door a tenant's key can push work
/// through must refuse it.
#[tokio::test]
async fn a_disabled_project_is_refused_at_every_ingest_door() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state.clone());

    // Sanity: all three doors work while the project is live.
    assert_eq!(ingest(&app, &key, event()).await.0, StatusCode::OK);
    let (s, _) = send(&app, "POST", "/v1/events/batch", &key, json!([event()])).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(&app, "POST", "/v1/traces", &key, otlp_export()).await;
    assert_eq!(s, StatusCode::OK);
    let live = store
        .list_events(TenantScope::Project("proj-a"), 10)
        .unwrap()
        .len();
    assert_eq!(live, 3, "one event per door");

    disable(&state, &store, "proj-a");

    for (method, uri, body) in [
        ("POST", "/v1/events", event()),
        ("POST", "/v1/events/batch", json!([event()])),
        ("POST", "/v1/traces", otlp_export()),
    ] {
        let (s, v) = send(&app, method, uri, &key, body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}: {v}");
        assert_eq!(v["error"]["code"], "project_disabled", "{uri}: {v}");
    }
    assert_eq!(
        store
            .list_events(TenantScope::Project("proj-a"), 10)
            .unwrap()
            .len(),
        live,
        "a disabled project stores nothing on any door"
    );
}

/// The second regression: one principal shape meant an ingest key shipped inside a client app could
/// read every stored prompt and completion in its project.
#[tokio::test]
async fn an_ingest_only_key_cannot_read_and_a_read_only_key_cannot_write() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a"); // creates the project
    let ingest_only = key_with(&store, "proj-a", vec![Scope::Ingest], None);
    let read_only = key_with(&store, "proj-a", vec![Scope::Read], None);
    let app = crate::build_router(state);

    assert_eq!(ingest(&app, &ingest_only, event()).await.0, StatusCode::OK);
    let (s, v) = get(&app, "/v1/events?project=proj-a", &ingest_only).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("needs 'read'"),
        "the denial has to name the missing capability: {v}"
    );

    let (s, v) = get(&app, "/v1/events?project=proj-a", &read_only).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = ingest(&app, &read_only, event()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(
        store
            .list_events(TenantScope::Project("proj-a"), 10)
            .unwrap()
            .len(),
        1
    );
}

/// An expired key is a *correct* secret that ran out of time, so it gets its own 401 code — and
/// must not be metered as a credential guess.
#[tokio::test]
async fn an_expired_key_is_401_key_expired_not_a_guess() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    let expired = key_with(
        &store,
        "proj-a",
        lighttrack_core::default_scopes(),
        Some(Utc::now() - Duration::seconds(1)),
    );
    let live = key_with(
        &store,
        "proj-a",
        lighttrack_core::default_scopes(),
        Some(Utc::now() + Duration::hours(1)),
    );
    let app = crate::build_router(state);

    let (s, v) = ingest(&app, &expired, event()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"]["code"], "key_expired", "{v}");
    let (s, v) = get(&app, "/v1/events?project=proj-a", &expired).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"]["code"], "key_expired", "{v}");

    // An expiry still in the future changes nothing.
    assert_eq!(ingest(&app, &live, event()).await.0, StatusCode::OK);
}

/// Rotation exists so a fleet can redeploy without a cliff: the successor works immediately, the
/// predecessor keeps working until its stamped deadline, and the deadline is durable state rather
/// than a background task a restart would drop.
#[tokio::test]
async fn rotation_mints_a_successor_and_gives_the_predecessor_a_deadline() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    let old = key_with(&store, "proj-a", vec![Scope::Ingest], None);
    let old_id = store
        .list_api_keys("proj-a")
        .unwrap()
        .into_iter()
        .find(|k| k.name == "scoped")
        .unwrap()
        .id;
    let app = crate::build_router(state);

    let (s, v) = send(
        &app,
        "POST",
        &format!("/v1/projects/proj-a/keys/{old_id}/rotate"),
        ADMIN,
        json!({ "grace_secs": 60 }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let successor = v["successor"]["key"].as_str().unwrap().to_string();
    assert_eq!(
        v["successor"]["scopes"],
        json!(["ingest"]),
        "the successor inherits the predecessor's capabilities, not the default: {v}"
    );
    assert!(v["predecessor"]["expires_at"].is_string(), "{v}");

    // Both work during the window — that is the entire point of a grace period.
    assert_eq!(ingest(&app, &successor, event()).await.0, StatusCode::OK);
    assert_eq!(ingest(&app, &old, event()).await.0, StatusCode::OK);

    // The window is durable state on the row, not a timer in this process.
    let stamped = store
        .list_api_keys("proj-a")
        .unwrap()
        .into_iter()
        .find(|k| k.id == old_id)
        .unwrap();
    assert!(stamped.expires_at.is_some());
    assert!(
        !stamped.revoked,
        "rotation expires a key, it does not revoke it"
    );

    // Close the window the way time would, and the predecessor is dead while the successor lives.
    assert!(store
        .set_api_key_expiry(&old_id, Some(Utc::now() - Duration::seconds(1)))
        .unwrap());
    let (s, v) = ingest(&app, &old, event()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"]["code"], "key_expired", "{v}");
    assert_eq!(ingest(&app, &successor, event()).await.0, StatusCode::OK);
}

/// `DELETE /v1/projects/:id` archives. The rows stay — they are what every cost report was computed
/// from — but the tenant stops accepting work, which is what a delete was reached for.
#[tokio::test]
async fn deleting_a_project_archives_it_and_keeps_the_rows() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    assert_eq!(ingest(&app, &key, event()).await.0, StatusCode::OK);

    let (s, v) = send(&app, "DELETE", "/v1/projects/proj-a", ADMIN, Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["enabled"], false, "{v}");
    let archived_at = v["archived_at"].as_str().expect("archived_at is stamped");

    assert_eq!(
        store
            .list_events(TenantScope::Project("proj-a"), 10)
            .unwrap()
            .len(),
        1,
        "archiving keeps the record the cost reports were built from"
    );
    let (s, v) = ingest(&app, &key, event()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "project_disabled", "{v}");

    // Idempotent: a second DELETE keeps the original archival instant rather than moving it.
    let (s, v) = send(&app, "DELETE", "/v1/projects/proj-a", ADMIN, Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["archived_at"], archived_at, "{v}");

    let (s, v) = send(&app, "DELETE", "/v1/projects/nope", ADMIN, Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
}

/// Minting is where an operator states intent, so the endpoint has to honour it exactly — and
/// refuse a typo rather than silently issuing a key that opens fewer doors than they asked for.
#[tokio::test]
async fn minting_honours_requested_scopes_and_refuses_an_unknown_one() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (s, v) = send(
        &app,
        "POST",
        "/v1/projects/proj-a/keys",
        ADMIN,
        json!({ "name": "shipped-app", "scopes": ["ingest"] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["scopes"], json!(["ingest"]), "{v}");
    let minted = v["key"].as_str().unwrap().to_string();
    assert_eq!(ingest(&app, &minted, event()).await.0, StatusCode::OK);
    assert_eq!(
        get(&app, "/v1/events?project=proj-a", &minted).await.0,
        StatusCode::FORBIDDEN,
        "an ingest-only key must not read the project's stored payloads"
    );

    let (s, v) = send(
        &app,
        "POST",
        "/v1/projects/proj-a/keys",
        ADMIN,
        json!({ "scopes": ["reed"] }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // An omitted `scopes` keeps the back-compat default, so an existing caller's script is unchanged.
    let (s, v) = send(&app, "POST", "/v1/projects/proj-a/keys", ADMIN, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["scopes"], json!(["ingest", "read"]), "{v}");
}
