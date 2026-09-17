//! The persistence class is a property of the project, so every door that makes an event durable
//! must apply it — including the one that builds its event server-side.
//!
//! Three of the four ingest doors reach the store through `events::prepare_event`, which calls
//! `redact::apply_policy` with the project's `Redaction`. The fourth — the relay settle report —
//! builds its event in `relay_result.rs`, resolves the same `ProjectPolicy`, reads only `enabled`
//! from it, and then passes `Redaction::None` to the scrub. Its payloads are gated instead on the
//! *action's own* `report_io` opt-in, which is a decision made per record by the emitter.
//!
//! The pair of assertions below pulls in opposite directions on purpose. One alone is satisfiable
//! by a change that is wrong in the other direction: dropping every relay payload passes the first
//! and fails the second; doing nothing passes the second and fails the first.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{LlmEvent, Redaction};
use lighttrack_store::{Scope as TenantScope, SqliteStore, Store};

use crate::redact::Redactor;
use crate::tests_ingest::{make_key_with_redaction, setup};

const PROMPT: &str = "Price SKU A-1";
const ANSWER: &str = "A-1 is $12";
const FINGERPRINT: &str = "5b4d1e0f5b4d1e0f5b4d1e0f5b4d1e0f5b4d1e0f5b4d1e0f5b4d1e0f5b4d1e0f";

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

fn fence_of(store: &Arc<SqliteStore>, id: &str) -> Value {
    serde_json::to_value(
        store
            .get_relay_task(TenantScope::Operator, id)
            .unwrap()
            .unwrap()
            .lease_fence,
    )
    .unwrap()
}

/// Enqueue, lease and settle one relay task in a project carrying `policy`, with the action opted
/// into reporting its content. Answers the single event the settle wrote.
///
/// `Redactor::off()` so the PII scrub cannot be mistaken for the persistence class: this test is
/// about the class alone, and the scrub has its own tests.
async fn settle_under(policy: Redaction) -> LlmEvent {
    let (state, store) = setup(Redactor::off());
    let pid = "proj-class";
    let key = make_key_with_redaction(&store, pid, policy);
    let app = crate::build_router(state);

    let (status, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key,
        Some(json!({ "action_type": "xprice/summary", "payload": { "sku": "A-1" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enqueue: {task}");
    let id = task["id"].as_str().unwrap().to_string();

    let (status, leased) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "lease: {leased}");

    let (status, settled) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/result"),
        "device-secret",
        Some(json!({
            "status": "succeeded",
            "result": { "text": ANSWER },
            "input": PROMPT,
            "output": ANSWER,
            "prompt_sha256": FINGERPRINT,
            "action_version": "3",
            "input_tokens": 10,
            "output_tokens": 5,
            "fence": fence_of(&store, &id),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "settle: {settled}");

    let mut events = store.list_events(TenantScope::Project(pid), 10).unwrap();
    assert_eq!(events.len(), 1, "the settle wrote exactly one event");
    let ev = events.remove(0);
    assert_eq!(
        ev.trace_id.as_deref(),
        Some(id.as_str()),
        "the trace id is the task id"
    );
    ev
}

/// **Separate when the class moves.** A project that says its payloads are never persisted must not
/// have them persisted by a door that builds the event itself, and one that says they are stored as
/// digests must get a digest.
#[tokio::test]
async fn the_persistence_class_reaches_the_server_built_door() {
    let dropped = settle_under(Redaction::Drop).await;
    let row = serde_json::to_string(&dropped).unwrap();
    assert!(
        dropped.input.is_none() && dropped.output.is_none(),
        "a `drop` project persisted a relay payload: {row}"
    );
    assert!(
        !row.contains(PROMPT) && !row.contains(ANSWER),
        "a `drop` project's row still carries the content: {row}"
    );

    let hashed = settle_under(Redaction::Hash).await;
    let row = serde_json::to_string(&hashed).unwrap();
    assert!(
        !row.contains(PROMPT) && !row.contains(ANSWER),
        "a `hash` project's row still carries plaintext: {row}"
    );
    let digest = hashed
        .input
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(
        digest.len(),
        64,
        "a `hash` project's relay payload is not a digest: {row}"
    );
}

/// **Agree when the class does not move.** Applying the class must not cost the door anything else:
/// a project that stores payloads still gets them verbatim, and every accounting, provenance and
/// identity field is the same under all three classes. This is the assertion a sledgehammer fails —
/// refusing the door, or dropping every relay payload regardless of the project, passes the test
/// above and fails this one.
#[tokio::test]
async fn nothing_but_the_payload_moves_with_the_class() {
    let kept = settle_under(Redaction::None).await;
    assert_eq!(
        kept.input.as_ref().and_then(|v| v.as_str()),
        Some(PROMPT),
        "a `none` project must still store what the device reported"
    );
    assert_eq!(kept.output.as_ref().and_then(|v| v.as_str()), Some(ANSWER));

    let dropped = settle_under(Redaction::Drop).await;
    let hashed = settle_under(Redaction::Hash).await;

    for (name, ev) in [("none", &kept), ("drop", &dropped), ("hash", &hashed)] {
        assert_eq!(ev.usage.input, 10, "{name}: reported input tokens");
        assert_eq!(ev.usage.output, 5, "{name}: reported output tokens");
        assert_eq!(ev.name.as_deref(), Some("relay-run"), "{name}: event name");
        assert_eq!(
            ev.status,
            lighttrack_core::Status::Success,
            "{name}: outcome"
        );
        assert!(
            ev.error.is_none(),
            "{name}: a succeeded run carries no error"
        );
        assert!(ev.tags.contains(&"relay".to_string()), "{name}: tags");
        assert_eq!(
            ev.metadata["action_type"], "xprice/summary",
            "{name}: action type"
        );
        assert_eq!(ev.metadata["action_version"], "3", "{name}: action version");
        assert_eq!(
            ev.metadata["prompt_sha256"], FINGERPRINT,
            "{name}: the prompt fingerprint is the metadata tier's own digest and survives every class"
        );
        assert_eq!(
            ev.metadata["task_id"].as_str(),
            ev.trace_id.as_deref(),
            "{name}: the task id and the trace id agree"
        );
        assert!(
            ev.cost_usd.is_some_and(|c| c > 0.0),
            "{name}: the run is priced"
        );
    }
}
