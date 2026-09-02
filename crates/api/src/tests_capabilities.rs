//! The wired capability surface: `/health` and `GET /v1/capabilities`.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

async fn get(app: &axum::Router, token: Option<&str>, uri: &str) -> (StatusCode, Value) {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The route exists so a client can find out what this deployment serves *before* building on it.
/// It must therefore be reachable by an ordinary project key, not just the admin key.
#[tokio::test]
async fn any_authenticated_principal_can_read_the_manifest() {
    let (state, store) = setup(Redactor::off());
    let project_key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = get(&app, Some(&project_key), "/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["backend"], "sqlite");
    assert_eq!(
        body["atomic_admission"], true,
        "SQLite admission is one critical section"
    );
    let surfaces = body["surfaces"].as_array().expect("surfaces array");
    assert!(surfaces.iter().any(|s| s == "traces"));
    assert!(
        body["unsupported"]
            .as_array()
            .expect("unsupported array")
            .is_empty(),
        "SQLite is the reference backend: nothing is refused"
    );

    let (status, _) = get(&app, Some("admin-secret"), "/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK, "admin reads it too");
}

#[tokio::test]
async fn the_manifest_is_not_public() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    let (status, _) = get(&app, Some("lt_not_a_key"), "/v1/capabilities").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// `/health` stays unauthenticated (a liveness probe is not a credential) and keeps `status: "ok"` —
/// the container healthcheck and `scripts/smoke.sh` read that field.
#[tokio::test]
async fn health_is_open_and_carries_the_surfaces() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    let (status, body) = get(&app, None, "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["backend"], "sqlite");
    assert!(
        body["capabilities"]["surfaces"]
            .as_array()
            .expect("surfaces")
            .iter()
            .any(|s| s == "events_core"),
        "the declared surfaces ride along on the endpoint operators already curl"
    );
}
