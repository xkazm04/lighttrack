//! End-to-end tests for the Collective hub ingest/leaderboard path over the **wired axum router**.
//!
//! These pin the hardening guarantees that no pure unit test covers: ingest is gated by the accept
//! flag; an unknown digest schema is rejected; the contributor identity is derived from the presented
//! bearer key (the body's id is ignored) so two keys land under two ids and one key can only replace
//! its own set; under-k buckets are dropped and counted; and a keyless push is refused unless the hub
//! opts into anonymous contributions.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::PriceBook;
use lighttrack_store::{SqliteStore, Store};

use crate::auth::AuthMode;
use crate::collective::Collective;
use crate::state::AppState;

/// App state over a fresh in-memory store, in **dev** auth mode (so a keyless or arbitrary-bearer
/// request reaches the collective handler), with a hub configured by the given knobs.
fn setup(accept: bool, allow_anon: bool, min_cases: u32) -> (AppState, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let dyn_store: Arc<dyn Store + Send + Sync> = store.clone();
    let state = AppState {
        store: dyn_store,
        prices: Arc::new(RwLock::new(PriceBook::new(HashMap::new()))),
        auth_mode: AuthMode::Dev,
        admin_key: None,
        relay_device_key: None,
        relay_flat_cost: 1.0,
        alerts: Arc::new(crate::alerts::Alerter::from_env()),
        redact: Arc::new(crate::redact::Redactor::off()),
        billing: Arc::new(lighttrack_billing::BillingRegistry::from_env()),
        collective: Arc::new(Collective {
            contributor_id: "anonymous".to_string(),
            accept,
            allow_anon,
            min_cases,
        }),
        seen_webhooks: Arc::new(crate::idempotency::SeenWebhooks::new(
            crate::idempotency::DEFAULT_CAPACITY,
        )),
        rejections: Arc::new(crate::rejections::RejectionLedger::new()),
    };
    (state, store)
}

fn entry(model: &str, q: f64, cases: u32) -> Value {
    json!({
        "provider": "anthropic", "model": model, "task_type": "qa",
        "quality": q, "pass_rate": q, "avg_cost_usd": 0.003,
        "p50_latency_ms": 900, "p95_latency_ms": 2000, "n_runs": 1, "n_cases": cases
    })
}

/// POST a digest to /v1/collective/ingest with an optional bearer token.
async fn ingest(app: &Router, token: Option<&str>, digest: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/collective/ingest")
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app.clone().oneshot(req.body(Body::from(digest.to_string())).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value =
        if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, v)
}

async fn leaderboard(app: &Router) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri("/v1/collective/leaderboard")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn ingest_refused_unless_accept_flag_set() {
    let (state, _) = setup(false, false, 5);
    let app = crate::build_router(state);
    let (status, body) =
        ingest(&app, Some("some-key"), json!({ "entries": [entry("haiku", 0.8, 10)] })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden", "{body}");
}

#[tokio::test]
async fn unknown_schema_version_is_rejected_400() {
    let (state, _) = setup(true, false, 5);
    let app = crate::build_router(state);
    let (status, body) = ingest(
        &app,
        Some("some-key"),
        json!({ "schema_version": 999, "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn identity_is_derived_from_the_key_not_the_body() {
    // A poster claims contributor_id "c-victim" in the body but presents its own key: the stored id
    // must be derived from the key, and must NOT be the claimed one.
    let (state, store) = setup(true, false, 5);
    let app = crate::build_router(state);
    let (status, ack) = ingest(
        &app,
        Some("attacker-key"),
        json!({ "contributor_id": "c-victim", "entries": [entry("haiku", 0.8, 10)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1);
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0].contributor_id, "c-victim", "body-supplied id must be ignored");
    assert!(stored[0].contributor_id.starts_with("c-"), "id is key-derived");
    assert_eq!(ack["contributor_id"], stored[0].contributor_id);
}

#[tokio::test]
async fn different_keys_land_under_different_ids_no_overwrite() {
    // Two keys both claim the same body id and the same model bucket. Because the identity is derived
    // from the key, they must NOT collide: two rows survive, not one overwriting the other.
    let (state, store) = setup(true, false, 5);
    let app = crate::build_router(state);
    let body = |q: f64| json!({ "contributor_id": "c-shared", "entries": [entry("haiku", q, 10)] });

    let (s1, _) = ingest(&app, Some("key-alpha"), body(0.7)).await;
    let (s2, _) = ingest(&app, Some("key-beta"), body(0.9)).await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));

    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 2, "distinct keys must not overwrite each other");
    let ids: std::collections::BTreeSet<_> = stored.iter().map(|e| e.contributor_id.clone()).collect();
    assert_eq!(ids.len(), 2, "two distinct derived ids");

    // The leaderboard therefore reports two contributors for the shared bucket.
    let (ls, lb) = leaderboard(&app).await;
    assert_eq!(ls, StatusCode::OK);
    assert_eq!(lb["rows"][0]["n_contributors"], 2, "{lb}");
}

#[tokio::test]
async fn same_key_replaces_its_own_set() {
    // Re-contributing under the same key replaces, never accretes.
    let (state, store) = setup(true, false, 5);
    let app = crate::build_router(state);
    let (_s, _) = ingest(
        &app,
        Some("key-alpha"),
        json!({ "entries": [entry("haiku", 0.7, 10), entry("sonnet", 0.8, 10)] }),
    )
    .await;
    assert_eq!(store.list_collective_entries().unwrap().len(), 2);
    // Second push from the same key with a single bucket → the dropped one must not linger.
    let (_s, _) = ingest(&app, Some("key-alpha"), json!({ "entries": [entry("haiku", 0.9, 20)] })).await;
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1, "re-contribution replaces the whole set");
    assert_eq!(stored[0].model, "haiku");
    assert_eq!(stored[0].n_cases, 20);
}

#[tokio::test]
async fn under_k_buckets_are_dropped_and_counted() {
    let (state, store) = setup(true, false, 5);
    let app = crate::build_router(state);
    // One bucket clears the floor (10 ≥ 5), one is below it (3 < 5).
    let (status, ack) = ingest(
        &app,
        Some("some-key"),
        json!({ "min_cases": 1, "entries": [entry("haiku", 0.8, 10), entry("sonnet", 0.9, 3)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["accepted"], 1, "{ack}");
    assert_eq!(ack["dropped_under_min"], 1, "{ack}");
    let stored = store.list_collective_entries().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].model, "haiku");
}

#[tokio::test]
async fn anonymous_push_refused_unless_allowed() {
    // Keyless push, hub does not allow anon → 403.
    let (state, _) = setup(true, false, 5);
    let app = crate::build_router(state);
    let (status, body) = ingest(&app, None, json!({ "entries": [entry("haiku", 0.8, 10)] })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Same push, hub opts into anon → accepted under the shared identity.
    let (state, store) = setup(true, true, 5);
    let app = crate::build_router(state);
    let (status, ack) = ingest(&app, None, json!({ "entries": [entry("haiku", 0.8, 10)] })).await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["contributor_id"], "anonymous");
    assert_eq!(store.list_collective_entries().unwrap()[0].contributor_id, "anonymous");
}
