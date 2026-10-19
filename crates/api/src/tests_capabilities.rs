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

    assert!(
        body["unsupported_routes"]
            .as_array()
            .expect("unsupported_routes array")
            .is_empty(),
        "nothing is refused here, so no route can be published as refused"
    );
    assert!(
        body["degraded_routes"]
            .as_array()
            .expect("degraded_routes array")
            .is_empty(),
        "and nothing is narrowed either"
    );

    let (status, _) = get(&app, Some("admin-secret"), "/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK, "admin reads it too");
}

/// The route-level consequence of a refusal, on the backends that actually have one.
///
/// The suite's only backend is SQLite, which refuses nothing — so this is the one API-level
/// assertion about a deployment other than the reference one. It needs no database: each backend's
/// manifest is a pure associated function of its type, which is what `crates/store/tests/
/// parity_doc.rs` already relies on.
#[test]
fn the_served_body_names_the_routes_each_backend_refuses() {
    use crate::capabilities::CapabilitiesBody;

    let pg = CapabilitiesBody::from(lighttrack_store_pg::PgStore::manifest());
    assert!(
        pg.unsupported.contains(&"use_cases")
            && pg.unsupported.contains(&"maintenance")
            && pg.unsupported.contains(&"metrics"),
        "Postgres refuses exactly these three: {:?}",
        pg.unsupported
    );
    assert!(
        pg.unsupported_routes
            .iter()
            .any(|r| r == "GET /v1/projects/{id}/use-cases"),
        "the use-case registry 501s on the backend carrying production traffic: {:?}",
        pg.unsupported_routes
    );
    // The finding this field exists to publish: two of those three refusals remove no route, so a
    // body that listed them as 501s would be telling the operator to expect a status code that
    // never arrives.
    assert!(
        !pg.unsupported_routes
            .iter()
            .any(|r| r.contains("/v1/storage/status")),
        "maintenance/metrics null two fields; they do not take the route out"
    );
    assert!(
        pg.degraded_routes
            .iter()
            .any(|d| d.route == "GET /v1/storage/status"),
        "and the narrowing is published rather than dropped"
    );

    let fs = CapabilitiesBody::from(lighttrack_store_firestore::FirestoreStore::manifest());
    for want in [
        "GET /v1/traces",
        "GET /v1/traces/{id}",
        "GET /v1/projects/{id}/redaction",
    ] {
        assert!(
            fs.unsupported_routes.iter().any(|r| r == want),
            "{want} must be published as refused on firestore: {:?}",
            fs.unsupported_routes
        );
    }
    let served: Vec<&str> = fs.surfaces.clone();
    for e in lighttrack_contract::endpoints() {
        if served.contains(&e.surface) {
            let n = format!("{} {}", e.method.as_str().to_uppercase(), e.path);
            assert!(
                !fs.unsupported_routes.contains(&n),
                "{} is on a served surface but was published as refused",
                e.id
            );
        }
    }
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
