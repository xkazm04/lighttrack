//! Router-level tests for the relay queue's auth boundaries: a project key enqueues only into its
//! own project and cannot lease or read across tenants; the enrolled device key (and only it,
//! besides admin) drives lease/result; and an idempotency key collapses duplicate enqueues.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

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

/// The fence the store currently holds for a task — what its holding device would report with.
fn fence_of(store: &std::sync::Arc<lighttrack_store::SqliteStore>, id: &str) -> Value {
    use lighttrack_store::Store;
    serde_json::to_value(store.get_relay_task(id).unwrap().unwrap().lease_fence).unwrap()
}

#[tokio::test]
async fn project_key_enqueue_is_forced_into_its_own_project() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // The body claims proj-b, but a project key always writes to its own project.
    let (status, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(
            json!({ "action_type": "xprice/summary", "project_id": "proj-b",
                     "payload": { "sku": "A-1" } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["project_id"], "proj-a");
    assert_eq!(task["status"], "queued");
    assert_eq!(task["max_attempts"], 4);
    assert_eq!(task["retry_interval_secs"], 18000);
}

#[tokio::test]
async fn device_key_leases_and_reports_project_keys_cannot() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let key_b = make_key(&store, "proj-b");
    let app = crate::build_router(state);

    let (_, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary" })),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();

    // A project key is not the device: lease and result are forbidden.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        &key_a,
        Some(json!({ "device": "pc" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The enrolled device key leases the due task…
    let (status, leased) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "device": "pc" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tasks = leased["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], id.as_str());
    assert_eq!(tasks[0]["attempts"], 1);
    // The lease answers with the renewal contract, so a device never has to guess a cadence
    // against a TTL the server clamped without telling it.
    let renew_secs = leased["renew_secs"].as_u64().unwrap();
    assert_eq!(renew_secs * 3, leased["lease_secs"].as_u64().unwrap());
    let fence = tasks[0]["lease_fence"].clone();
    assert!(
        !fence.is_null(),
        "a lease stamps a fence the device reports with"
    );

    // …and settles it, carrying that fence.
    let (status, settled) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/result"),
        "device-secret",
        Some(json!({ "status": "succeeded", "result": { "ok": true }, "fence": fence })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled["status"], "succeeded");

    // The owner reads its task back; a foreign project key cannot.
    let (status, got) = call(&app, "GET", &format!("/v1/relay/tasks/{id}"), &key_a, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["result"]["ok"], true);
    let (status, _) = call(&app, "GET", &format!("/v1/relay/tasks/{id}"), &key_b, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn terminal_settle_logs_one_flat_cost_event_deferred_none() {
    use lighttrack_store::Store;

    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (_, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary", "source": "xprice-app" })),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();

    // Deferred settle (rate limit): no Claude run happened, so no event.
    let lease = json!({ "device": "pc" });
    call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(lease.clone()),
    )
    .await;
    let fence = fence_of(&store, &id);
    call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/result"),
        "device-secret",
        Some(
            json!({ "status": "deferred", "error": "window", "retry_after_secs": 0,
                     "fence": fence }),
        ),
    )
    .await;
    assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty());

    // Successful settle: exactly one event at the flat price, traced by task id.
    call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(lease),
    )
    .await;
    let report = json!({ "status": "succeeded", "result": { "ok": true }, "model": "claude-sonnet-5",
                         "input_tokens": 1200, "output_tokens": 300, "latency_ms": 4500,
                         "fence": fence_of(&store, &id) });
    call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/result"),
        "device-secret",
        Some(report.clone()),
    )
    .await;
    let events = store.list_events(Some("proj-a"), 10).unwrap();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.cost_usd, Some(1.0));
    assert_eq!(ev.trace_id.as_deref(), Some(id.as_str()));
    assert_eq!(ev.model, "claude-sonnet-5");
    assert_eq!(ev.usage.input, 1200);
    assert_eq!(ev.source.as_deref(), Some("xprice-app"));
    assert_eq!(ev.metadata["action_type"], "xprice/summary");

    // A duplicate report of the already-settled task is refused (409), and must not double-log.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/result"),
        "device-secret",
        Some(report),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 1);
}

/// The router-level shape of the fence: a device whose lease was reclaimed is told 409 on renew,
/// progress AND result — and its run is never recorded against the task its successor now holds.
///
/// This is the whole point of M7's relay half. Without the fence, that late `POST .../result` is a
/// 200 that overwrites a run in progress and logs a cost event for a task somebody else owns.
#[tokio::test]
async fn a_reclaimed_device_is_refused_on_every_door_and_logs_nothing() {
    use lighttrack_store::Store;

    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (_, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary", "max_attempts": 4 })),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();

    // Device 1 leases it, proves it is alive, and reports progress.
    let (_, first) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "device": "pc-1" })),
    )
    .await;
    let stale_fence = first["tasks"][0]["lease_fence"].clone();
    let (status, held) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/renew"),
        "device-secret",
        Some(json!({ "fence": stale_fence })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(held["outcome"], "held");
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/progress"),
        "device-secret",
        Some(json!({ "fence": stale_fence, "progress": "step 2 of 5" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, got) = call(&app, "GET", &format!("/v1/relay/tasks/{id}"), &key_a, None).await;
    assert_eq!(got["progress"], "step 2 of 5");

    // Device 1 goes silent. Expire its lease at the store and let device 2 reclaim the task.
    store
        .renew_relay_lease(&id, serde_json::from_value(stale_fence.clone()).unwrap(), 0)
        .unwrap();
    let (_, second) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "device": "pc-2" })),
    )
    .await;
    assert_eq!(second["tasks"].as_array().unwrap().len(), 1);
    assert_ne!(second["tasks"][0]["lease_fence"], stale_fence);

    // Device 1 comes back. Every door refuses it, and nothing it says is recorded.
    for (path, body) in [
        (
            format!("/v1/relay/tasks/{id}/renew"),
            json!({ "fence": stale_fence }),
        ),
        (
            format!("/v1/relay/tasks/{id}/progress"),
            json!({ "fence": stale_fence, "progress": "still going" }),
        ),
        (
            format!("/v1/relay/tasks/{id}/result"),
            json!({ "status": "succeeded", "result": { "from": "the zombie" },
                    "fence": stale_fence }),
        ),
    ] {
        let (status, _) = call(&app, "POST", &path, "device-secret", Some(body)).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{path} must refuse a lost lease"
        );
    }
    let (_, got) = call(&app, "GET", &format!("/v1/relay/tasks/{id}"), &key_a, None).await;
    assert_eq!(got["status"], "leased", "the successor still holds it");
    assert!(
        got["result"].is_null(),
        "the zombie's result was not written"
    );
    assert!(
        store.list_events(Some("proj-a"), 10).unwrap().is_empty(),
        "a refused report must not log a cost event against someone else's run"
    );
}

/// Cancel: the task's own project key may stop it, a foreign key may not, and cancelling something
/// already terminal is a 409 rather than a comfortable lie.
#[tokio::test]
async fn cancel_is_reachable_by_the_owner_and_never_claims_a_false_stop() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let key_b = make_key(&store, "proj-b");
    let app = crate::build_router(state);

    let (_, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary" })),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();

    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/cancel"),
        &key_b,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not that project's task");

    let (status, out) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/cancel"),
        &key_a,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["outcome"], "cancelled");

    // A cancelled task is never handed to a device…
    let (_, leased) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "device": "pc" })),
    )
    .await;
    assert!(leased["tasks"].as_array().unwrap().is_empty());
    // …and re-cancelling it does not pretend to have stopped anything.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/cancel"),
        &key_a,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn exhausted_failure_dead_letters_and_long_poll_waits() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // max_attempts = 1: the first failed attempt dead-letters straight away.
    let (_, task) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary", "max_attempts": 1 })),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();
    call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "device": "pc" })),
    )
    .await;
    let (status, dead) = call(
        &app,
        "POST",
        &format!("/v1/relay/tasks/{id}/result"),
        "device-secret",
        Some(json!({ "status": "failed", "error": "boom", "fence": fence_of(&store, &id) })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dead["status"], "dead");
    assert_eq!(dead["error"], "boom");

    // Long-poll on an empty queue holds the request ~wait_secs before answering empty.
    let t0 = std::time::Instant::now();
    let (status, leased) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "device": "pc", "wait_secs": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(leased["tasks"].as_array().unwrap().is_empty());
    assert!(t0.elapsed() >= std::time::Duration::from_secs(1));
}

#[tokio::test]
async fn idempotency_key_collapses_duplicate_enqueues() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let body = json!({ "action_type": "xprice/summary", "idempotency_key": "order-42" });
    let (_, first) = call(&app, "POST", "/v1/relay/tasks", &key_a, Some(body.clone())).await;
    let (status, second) = call(&app, "POST", "/v1/relay/tasks", &key_a, Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["id"], second["id"]);

    let (_, listed) = call(&app, "GET", "/v1/relay/tasks?status=queued", &key_a, None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// M18 — the device fleet: enrolment, capability-routed leases, and the admission verdict.
// ---------------------------------------------------------------------------------------------

/// Enrol a device and return `(id, raw key)`. The key is returned exactly once, here.
async fn enrol(app: &Router, name: &str, capabilities: Value) -> (String, String) {
    let (status, body) = call(
        app,
        "POST",
        "/v1/relay/devices",
        "admin-secret",
        Some(json!({ "name": name, "capabilities": capabilities })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enrolment failed: {body}");
    (
        body["id"].as_str().unwrap().to_string(),
        body["key"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn enrolment_shows_the_key_once_and_never_leaks_its_digest() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);

    let (status, created) = call(
        &app,
        "POST",
        "/v1/relay/devices",
        "admin-secret",
        Some(json!({ "name": "laptop", "capabilities": ["xprice/*"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let key = created["key"].as_str().expect("the key is shown once");
    assert!(
        key.starts_with("ltd_"),
        "device keys carry their own scheme: {key}"
    );
    assert_eq!(
        created["key_hash"], "",
        "the stored digest never leaves the DB"
    );
    assert_eq!(created["capabilities"][0], "xprice/*");

    // The fleet listing carries liveness and — crucially — not the key.
    let (status, fleet) = call(&app, "GET", "/v1/relay/devices", "admin-secret", None).await;
    assert_eq!(status, StatusCode::OK);
    let d = &fleet.as_array().unwrap()[0];
    assert!(
        d.get("key").is_none(),
        "a key appears at enrolment and nowhere else"
    );
    assert_eq!(d["key_hash"], "");
    assert_eq!(
        d["online"], false,
        "a device that has never leased has never proved it is alive"
    );

    // The fleet is operator infrastructure, not one project's data.
    let (status, _) = call(&app, "GET", "/v1/relay/devices", "not-the-admin-key", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_lease_takes_only_what_the_device_advertises_and_leaves_the_rest_untouched() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (device_id, device_key) = enrol(&app, "xprice-laptop", json!(["xprice/*"])).await;
    // A second device covers the other namespace, so both tasks are admissible at the door and the
    // only thing being tested below is ROUTING, not admission.
    enrol(&app, "ops-box", json!(["ops/*"])).await;

    let (_, mine) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary" })),
    )
    .await;
    let (_, theirs) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "ops/nightly" })),
    )
    .await;

    let (status, leased) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        &device_key,
        Some(json!({ "capabilities": ["xprice/*"], "agent_version": "1.2.3", "max": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tasks = leased["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "only the advertised namespace is leasable");
    assert_eq!(tasks[0]["id"], mine["id"]);
    assert_eq!(
        tasks[0]["device"], device_id,
        "the leasing device is the one the KEY names, not one the body asserted"
    );

    // The task it cannot run is not merely unreturned — it is untouched, so no attempt was burned
    // and no fence was stamped. That is the difference between routing and post-filtering.
    let (_, other) = call(
        &app,
        "GET",
        &format!("/v1/relay/tasks/{}", theirs["id"].as_str().unwrap()),
        &key_a,
        None,
    )
    .await;
    assert_eq!(other["status"], "queued");
    assert_eq!(other["attempts"], 0);

    // The lease is also the heartbeat: liveness and the reported agent version land on the device.
    let (_, fleet) = call(&app, "GET", "/v1/relay/devices", "admin-secret", None).await;
    let me = fleet
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == device_id.as_str())
        .expect("our device in the fleet");
    assert_eq!(me["online"], true);
    assert_eq!(me["agent_version"], "1.2.3");
}

#[tokio::test]
async fn an_action_nothing_advertises_is_refused_at_the_door_not_hours_later() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // With NOBODY enrolled the enqueue must still be accepted: that is the legacy shared-key
    // deployment, and refusing its traffic would be this feature breaking the relay it hardens.
    let (status, accepted) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accepted["admission"]["verdict"], "queued");
    assert_eq!(accepted["admission"]["eligible_devices"], 0);

    enrol(&app, "xprice-laptop", json!(["xprice/*"])).await;

    // Now a fleet exists. An action inside it is queued, and says how much of the fleet can run it.
    let (status, ok) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/reprice" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ok["admission"]["eligible_devices"], 1);

    // …and one outside it is refused, with a reason that names the action and the fix. Before M18
    // this was accepted, handed to a device with no such action four times, and dead-lettered
    // roughly twenty hours later.
    let (status, refused) = call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xpricey/typo" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused["error"]["code"], "relay_unroutable");
    let msg = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("xpricey/typo"), "{msg}");

    // Nothing was stored: a queue entry nothing can lease is a slow-motion dead letter.
    let (_, listed) = call(&app, "GET", "/v1/relay/tasks?status=queued", &key_a, None).await;
    let stored: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["action_type"].as_str().unwrap())
        .collect();
    assert!(!stored.contains(&"xpricey/typo"), "{stored:?}");
}

#[tokio::test]
async fn a_revoked_device_authenticates_nothing() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (device_id, device_key) = enrol(&app, "laptop", json!(["xprice/*"])).await;
    call(
        &app,
        "POST",
        "/v1/relay/tasks",
        &key_a,
        Some(json!({ "action_type": "xprice/summary" })),
    )
    .await;

    let (status, revoked) = call(
        &app,
        "DELETE",
        &format!("/v1/relay/devices/{device_id}"),
        "admin-secret",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        revoked["revoked"], true,
        "revocation is read back, not reported blind — it is a security action"
    );

    // Its key no longer leases. This is the whole point of per-device keys: one machine can be cut
    // off without re-keying the fleet.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        &device_key,
        Some(json!({ "capabilities": ["xprice/*"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Revoking something that does not exist says so rather than reporting success.
    let (status, _) = call(
        &app,
        "DELETE",
        "/v1/relay/devices/no-such-device",
        "admin-secret",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_legacy_shared_key_still_leases_everything() {
    // The deprecation contract: a fleet that has not enrolled anything keeps working exactly as it
    // did, unfiltered. A relay that stopped leasing the moment this shipped would be the feature
    // breaking the thing it hardens.
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    for action in ["xprice/summary", "ops/nightly"] {
        call(
            &app,
            "POST",
            "/v1/relay/tasks",
            &key_a,
            Some(json!({ "action_type": action })),
        )
        .await;
    }

    // No `capabilities` in the body — which is what a pre-M18 agent sends.
    let (status, leased) = call(
        &app,
        "POST",
        "/v1/relay/lease",
        "device-secret",
        Some(json!({ "max": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        leased["tasks"].as_array().unwrap().len(),
        2,
        "an unadvertised device leases everything, as it always did"
    );
    // …and the `device` an old agent asserts in the body is ignored rather than trusted.
    let stamped = leased["tasks"][0]["device"].as_str().unwrap();
    assert_eq!(stamped, "default");
}
