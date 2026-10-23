//! Ingest-door tests for the `cached_input ⊆ input` invariant.
//!
//! [`lighttrack_core::TokenUsage`] states the convention in prose — `input` is the whole prompt the
//! provider counted and `cached_input` is a part of it — and the price book bills the two halves at
//! different rates. A record that reports the cached tokens *beside* rather than *within* the total
//! therefore does not merely look odd: the residual saturates to zero and the **entire** prompt
//! prices at the cached rate, which is an under-bill in the one direction nobody audits.
//!
//! These drive the wired router (`crate::build_router`), so what is pinned here is the boundary
//! behaviour of all three ingest doors at once: the single-event door answers 400 with its own code,
//! the batch door answers the same code in that one item's result while its siblings are accepted,
//! and nothing violating is stored by either.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_store::{Scope as TenantScope, Store};

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

/// POST a batch through the real router; returns the status and parsed JSON.
async fn ingest_batch(app: &Router, token: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/events/batch")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn event(id: &str, input: u64, cached: Option<u64>) -> Value {
    let mut usage = json!({ "input": input, "output": 10 });
    if let Some(c) = cached {
        usage["cached_input"] = json!(c);
    }
    json!({
        "id": id,
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": usage
    })
}

#[tokio::test]
async fn single_door_refuses_a_cached_subcount_larger_than_its_total() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = ingest(&app, &key, event("violating", 100, Some(400))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["code"], "usage_subcount_exceeds_total",
        "its own code, not a generic bad_request — a sender has to be able to branch on it: {body}"
    );
    assert!(
        store
            .list_events(TenantScope::Project("proj-a"), 10)
            .unwrap()
            .is_empty(),
        "nothing violating is stored"
    );
}

#[tokio::test]
async fn the_legal_fully_cached_prompt_is_still_accepted() {
    // GUARD: the rule is one-sided. `cached == input` is the ordinary warm-cache case and must stay
    // acceptable, and an absent `cached_input` must not be coerced to `Some(0)` on its way through.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (s_full, b_full) = ingest(&app, &key, event("fully-cached", 500, Some(500))).await;
    assert_eq!(s_full, StatusCode::OK, "{b_full}");
    let (s_none, b_none) = ingest(&app, &key, event("uncached", 500, None)).await;
    assert_eq!(s_none, StatusCode::OK, "{b_none}");

    let rows = store
        .list_events(TenantScope::Project("proj-a"), 10)
        .unwrap();
    let full = rows.iter().find(|e| e.id == "fully-cached").unwrap();
    assert_eq!(full.usage.cached_input, Some(500));
    let none = rows.iter().find(|e| e.id == "uncached").unwrap();
    assert_eq!(
        none.usage.cached_input, None,
        "tri-state: `None` is never stored as `Some(0)`"
    );
}

#[tokio::test]
async fn batch_door_refuses_only_the_violating_item() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = ingest_batch(
        &app,
        &key,
        json!([
            event("ok-1", 100, Some(40)),
            event("violating", 100, Some(400)),
            event("ok-2", 100, None),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "multi-status, as always: {body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{body}");
    assert_eq!(results[0]["status"], "accepted", "{body}");
    assert_eq!(results[1]["status"], "invalid", "{body}");
    assert_eq!(
        results[1]["code"], "usage_subcount_exceeds_total",
        "the same code the single door returns: {body}"
    );
    assert_eq!(results[2]["status"], "accepted", "{body}");

    let ids: Vec<String> = store
        .list_events(TenantScope::Project("proj-a"), 10)
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert!(ids.contains(&"ok-1".to_string()), "{ids:?}");
    assert!(ids.contains(&"ok-2".to_string()), "{ids:?}");
    assert!(
        !ids.contains(&"violating".to_string()),
        "the partial-success contract is unchanged, and the bad item is not stored: {ids:?}"
    );
}
