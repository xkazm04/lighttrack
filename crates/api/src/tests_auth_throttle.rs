//! Router-level tests for the failed-authentication throttle.
//!
//! These drive the **wired** `crate::build_router` — including the `auth_throttle::source_scope`
//! layer — so they pin the whole path an attacker actually walks: peer address → failure budget →
//! 429 with a `Retry-After`. Style follows `tests_ingest` / `tests_dev_mode`, with one addition: the
//! request carries a `ConnectInfo` extension, which is exactly what `main` installs on the real
//! server via `into_make_service_with_connect_info`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::auth::AuthMode;
use crate::auth_throttle::AuthThrottle;
use crate::redact::Redactor;
use crate::state::AppState;
use crate::tests_ingest::{make_key, setup};
use lighttrack_store::Scope as TenantScope;

/// App state with an explicit throttle, so a test spends a budget in a handful of requests rather
/// than the production default of ten a minute.
fn state_with(
    max_failures: u32,
    window: Duration,
) -> (AppState, Arc<lighttrack_store::SqliteStore>) {
    let (mut state, store) = setup(Redactor::off());
    state.auth_throttle = Arc::new(AuthThrottle::new(max_failures, window, 4096, 0));
    (state, store)
}

/// One request from `peer`, with an optional bearer token and `X-Forwarded-For`.
async fn call(
    app: &Router,
    peer: &str,
    token: Option<&str>,
    xff: Option<&str>,
) -> (StatusCode, Option<String>, Value) {
    let mut b = Request::builder().method("GET").uri("/v1/prices");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(v) = xff {
        b = b.header("x-forwarded-for", v);
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let retry = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, retry, body)
}

const PEER: &str = "203.0.113.9:51000";
const OTHER: &str = "198.51.100.4:51000";

#[tokio::test]
async fn the_throttle_engages_after_the_configured_number_of_failures() {
    let (state, _store) = state_with(3, Duration::from_secs(60));
    let app = crate::build_router(state);

    // The budget: three guesses answered honestly.
    for i in 0..3 {
        let (status, retry, body) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}: {body}");
        assert_eq!(body["error"]["code"], "unauthorized", "{body}");
        assert!(retry.is_none(), "a 401 is not a scheduled retry");
    }

    // The fourth is refused before the credential is even compared.
    let (status, retry, body) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "rate_limited", "{body}");
    let secs: u64 = retry
        .expect("a 429 must carry Retry-After or the client is guessing")
        .parse()
        .unwrap();
    assert!((1..=61).contains(&secs), "retry-after was {secs}s");
    // The message must not read like a spend limit — an operator staring at a 429 needs to know
    // which budget they blew.
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("failed authentication"), "{body}");

    // A *different* address is untouched: the block is per source, not global.
    let (status, _, _) = call(&app, OTHER, Some("lt_deadbeef_guess"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_valid_credential_is_never_penalised_by_its_own_traffic() {
    let (state, store) = state_with(3, Duration::from_secs(60));
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // Far past the failure budget in successful requests: never throttled, never counted.
    for i in 0..20 {
        let (status, _, body) = call(&app, PEER, Some(&key), None).await;
        assert_eq!(status, StatusCode::OK, "request {i}: {body}");
    }

    // And a success below the threshold restores the budget, so a client that fat-fingers a key and
    // then gets it right is not carrying the failures around for the rest of the window.
    for _ in 0..2 {
        let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, _) = call(&app, PEER, Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "the good key must still work");
    for _ in 0..3 {
        let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the success reset the count, so the budget is whole again"
        );
    }
}

#[tokio::test]
async fn an_active_block_refuses_the_good_key_too_and_then_decays() {
    // The honest consequence of checking *before* comparing: while a source is blocked, everything
    // from it is blocked, including a valid credential. That is what makes it a throttle rather than
    // a relabelling of the 401 — and it is why the window is short by default.
    let (state, store) = state_with(2, Duration::from_millis(300));
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    for _ in 0..2 {
        let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, retry, _) = call(&app, PEER, Some(&key), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(retry.is_some());

    // The window decays on its own — no restart, no operator action.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, _, body) = call(&app, PEER, Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "the window must roll: {body}");
    let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "and the budget must be whole again after the roll"
    );
}

#[tokio::test]
async fn a_spoofed_x_forwarded_for_neither_evades_the_block_nor_poisons_a_victim() {
    // The default posture, and the reason it is the default: with hops unconfigured the header is
    // decoration. An attacker cannot rotate out of their own bucket by inventing addresses, and
    // cannot push a victim's address into one.
    let (state, _store) = state_with(2, Duration::from_secs(60));
    let app = crate::build_router(state);

    for i in 0..2 {
        let (status, _, _) = call(
            &app,
            PEER,
            Some("lt_deadbeef_guess"),
            Some(&format!("10.0.0.{i}")),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), Some("10.0.0.99")).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a fresh forged XFF must not buy a fresh budget"
    );

    // The address the attacker forged is not itself throttled — nobody was framed.
    let (status, _, _) = call(&app, "10.0.0.99:4000", Some("lt_deadbeef_guess"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn dev_mode_records_nothing_and_throttles_nobody() {
    // In dev mode every unrecognised token authenticates as `Principal::Dev`, so there are no
    // failures to meter. The throttle must stay completely out of the way — no counters, no noise.
    let (mut state, _store) = state_with(2, Duration::from_secs(60));
    state.auth_mode = AuthMode::Dev;
    let throttle = state.auth_throttle.clone();
    let app = crate::build_router(state);

    for i in 0..25 {
        let (status, _, body) = call(&app, PEER, Some(&format!("garbage-{i}")), None).await;
        assert_eq!(status, StatusCode::OK, "attempt {i}: {body}");
    }
    let (status, _, _) = call(&app, PEER, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        throttle.tracked(),
        0,
        "dev mode must not populate the throttle map at all"
    );
}

#[tokio::test]
async fn the_tracked_source_map_stays_bounded_under_address_rotation() {
    // The end-to-end version of the unit bound: an attacker rotating source addresses through the
    // real router must not grow the map without limit — an unbounded map keyed by attacker-chosen
    // input is itself the denial of service.
    let (mut state, _store) = state_with(1, Duration::from_secs(60));
    state.auth_throttle = Arc::new(AuthThrottle::new(1, Duration::from_secs(60), 16, 0));
    let throttle = state.auth_throttle.clone();
    let app = crate::build_router(state);

    let mut last = String::new();
    for i in 0..200 {
        last = format!("10.{}.{}.{}:4000", i / 256, i % 256, (i * 7) % 256);
        let (status, _, _) = call(&app, &last, Some("lt_deadbeef_guess"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        throttle.tracked(),
        16,
        "200 distinct sources must still fit in a 16-entry cap"
    );
    // Full does not mean blind: the most recent offender is still being counted, so filling the map
    // from a botnet does not buy an attacker a free budget on the next address.
    let (status, _, _) = call(&app, &last, Some("lt_x_y"), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn unauthenticated_routes_are_untouched_by_a_block() {
    // `/health` never authenticates, so it must keep answering while a source is blocked — an
    // operator's liveness probe is not a credential guess.
    let (state, _store) = state_with(1, Duration::from_secs(60));
    let app = crate::build_router(state);

    let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let mut req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(PEER.parse::<std::net::SocketAddr>().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn ingest_from_a_blocked_source_is_refused_before_it_reaches_the_store() {
    // The route that matters most: a throttled source must not be able to write, and the refusal has
    // to be the same 429 contract the SDKs already back off on.
    let (state, store) = state_with(1, Duration::from_secs(60));
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, _, _) = call(&app, PEER, Some("lt_deadbeef_guess"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let event = json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 }
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/events")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(event.to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(PEER.parse::<std::net::SocketAddr>().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().get("retry-after").is_some());

    use lighttrack_store::Store;
    assert!(
        store
            .list_events(TenantScope::Project("proj-a"), 10)
            .unwrap()
            .is_empty(),
        "a blocked source must not reach the store"
    );
}
