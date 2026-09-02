//! The redaction stamp end to end: ingest writes it, a client cannot forge it, and
//! `GET /v1/projects/:id/redaction` groups the stored rows by it.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::LlmEvent;
use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};
use lighttrack_store::Scope as TenantScope;

async fn call(app: &axum::Router, method: &str, uri: &str, token: &str, body: Value) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn get_json(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn event(input: &str) -> Value {
    json!({
        "provider": "anthropic", "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 },
        "input": input,
    })
}

/// Every ingested row carries a stamp, whether or not the scrub found anything — and the stamp
/// names the rule set that produced it, so two counts written across a rule change are comparable.
#[tokio::test]
async fn ingest_stamps_every_row_with_what_the_boundary_did() {
    let (state, store) = setup(Redactor::all());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/events",
            &key,
            event("mail me at a@b.com")
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "POST", "/v1/events", &key, event("nothing to see")).await,
        StatusCode::OK
    );

    let events: Vec<LlmEvent> = store
        .list_events(TenantScope::Project("proj-a"), 10)
        .unwrap();
    assert_eq!(events.len(), 2);
    let stamps: Vec<_> = events.iter().map(|e| e.redaction()).collect();
    assert!(
        stamps.iter().all(|s| s.is_some()),
        "no ingested row may be left unaccounted for: {stamps:?}"
    );
    let fp = lighttrack_anon::rules_fingerprint();
    assert!(stamps.iter().flatten().all(|s| s.scrub && s.rules == fp));
    let spans: Vec<u32> = stamps.iter().flatten().map(|s| s.spans).collect();
    assert!(
        spans.contains(&1) && spans.contains(&0),
        "the scrubbed row records its span, the clean one records zero: {spans:?}"
    );
}

/// With the scrub off the row is still stamped — `scrub: false` is a decision recorded, and the
/// absence of a stamp would be the thing that makes a database unauditable.
#[tokio::test]
async fn a_row_nobody_scrubbed_is_stamped_as_such_rather_than_left_blank() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    assert_eq!(
        call(&app, "POST", "/v1/events", &key, event("a@b.com")).await,
        StatusCode::OK
    );

    let e = &store
        .list_events(TenantScope::Project("proj-a"), 10)
        .unwrap()[0];
    let stamp = e.redaction().expect("stamped even with the scrub off");
    assert!(!stamp.scrub);
    assert_eq!(stamp.spans, 0);
    assert!(stamp.rules.is_empty(), "no rules ran, so none are claimed");
    assert!(
        e.input
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("a@b.com"),
        "…and the text really was stored verbatim, which is what the stamp says"
    );
}

/// The stamp is server-owned. A caller that sends its own gets it stripped, exactly like
/// `api_key_id` — otherwise "was this scrubbed" would be answerable by the party being audited.
#[tokio::test]
async fn a_client_cannot_forge_the_stamp() {
    let (state, store) = setup(Redactor::all());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let mut body = event("card 4111 1111 1111 1111");
    body["metadata"] = json!({
        "customer_id": "acme",
        "redaction": { "policy": "drop", "scrub": true, "spans": 999, "rules": "forged" }
    });
    assert_eq!(
        call(&app, "POST", "/v1/events", &key, body).await,
        StatusCode::OK
    );

    let e = &store
        .list_events(TenantScope::Project("proj-a"), 10)
        .unwrap()[0];
    let stamp = e.redaction().expect("the server's own stamp");
    assert_ne!(stamp.spans, 999, "the forged count did not survive");
    assert_eq!(stamp.rules, lighttrack_anon::rules_fingerprint());
    assert_eq!(
        e.customer_id(),
        Some("acme"),
        "the rest of the client's metadata is untouched"
    );
}

/// The route reports the stored posture, and reports "we do not know" as its own number rather
/// than folding unstamped rows in with deliberately-unscrubbed ones.
#[tokio::test]
async fn the_posture_route_groups_stored_rows_and_names_the_unaccounted_for() {
    let (state, store) = setup(Redactor::all());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state.clone());

    for payload in ["a@b.com", "clean text", "clean text too"] {
        assert_eq!(
            call(&app, "POST", "/v1/events", &key, event(payload)).await,
            StatusCode::OK
        );
    }
    // A row that predates the stamp, written straight to the store as an older binary would have.
    let legacy: LlmEvent = serde_json::from_value(json!({
        "project_id": "proj-a", "provider": "anthropic", "model": "claude-haiku-4-5"
    }))
    .unwrap();
    store.insert_event(&legacy).unwrap();

    let (status, body) = get_json(&app, "/v1/projects/proj-a/redaction", &key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_events"], 4);
    assert_eq!(
        body["unaccounted_events"], 1,
        "the pre-stamp row is named, not absorbed: {body}"
    );
    assert_eq!(body["current_rules"], lighttrack_anon::rules_fingerprint());
    let groups = body["groups"].as_array().expect("groups");
    assert_eq!(
        groups.len(),
        3,
        "unstamped / 0 spans / 1 span are three groups: {groups:?}"
    );

    // A project key may read only its own posture.
    let other = make_key(&store, "proj-b");
    let (status, _) = get_json(&app, "/v1/projects/proj-a/redaction", &other).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
