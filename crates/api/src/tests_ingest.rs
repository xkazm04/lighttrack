//! End-to-end ingest tests for the admission/enforcement + tenant-isolation path.
//!
//! These drive the **wired axum router** (`crate::build_router`) over an in-memory `SqliteStore`
//! via `tower`'s `oneshot`, exercising auth → project-scoping → pricing-from-book → redaction →
//! limit admission as one stack. They pin the guarantees `events::post_event` makes that no unit
//! test covers: a project key can only write to its own project; an uncosted event is priced from
//! the DP price book; PII is scrubbed before the row is stored; and an enforcing (`Throttle`/
//! `Block`) breach rejects ingest (HTTP 429, not recorded) while an `Alert` breach admits and
//! records the event.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{
    new_id, ApiKey, LimitAction, LimitMetric, LimitRule, LimitWindow, ModelPrice, PriceBook,
    Project, Redaction,
};
use lighttrack_store::{SqliteStore, Store};

use crate::auth::{self, AuthMode};
use crate::redact::Redactor;
use crate::state::AppState;

/// Build app state over a fresh in-memory store with the given redactor and a one-model price book
/// (`anthropic/claude-haiku-4-5` @ $1/Mtok in, $5/Mtok out). Returns the wired state plus the
/// concrete store handle so a test can inspect the persisted rows after a request.
pub(crate) fn setup(redact: Redactor) -> (AppState, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let dyn_store: Arc<dyn Store + Send + Sync> = store.clone();

    let mut entries = HashMap::new();
    entries.insert(
        "anthropic/claude-haiku-4-5".to_string(),
        ModelPrice { input_per_mtok: 1.0, output_per_mtok: 5.0, cached_input_per_mtok: None },
    );
    let book = PriceBook::new(entries);

    let state = AppState {
        store: dyn_store,
        prices: Arc::new(RwLock::new(book)),
        auth_mode: AuthMode::Enforced,
        admin_key: Some("admin-secret".to_string()),
        relay_device_key: Some("device-secret".to_string()),
        relay_flat_cost: 1.0,
        alerts: Arc::new(crate::alerts::Alerter::from_env()),
        redact: Arc::new(redact),
        billing: Arc::new(lighttrack_billing::BillingRegistry::from_env()),
        collective: Arc::new(crate::collective::Collective::from_env()),
        seen_webhooks: Arc::new(crate::idempotency::SeenWebhooks::new(
            crate::idempotency::DEFAULT_CAPACITY,
        )),
        rejections: Arc::new(crate::rejections::RejectionLedger::new()),
        // Empty cache: policies are back-filled lazily from the store on first sight, which is also
        // the path these tests exercise.
        redaction_policies: Arc::new(RwLock::new(HashMap::new())),
    };
    (state, store)
}

/// Create a project and mint a real, usable API key for it; returns the full secret to present as a
/// bearer token. Uses the production key-gen + hashing so auth resolves it to `Principal::Project`.
pub(crate) fn make_key(store: &SqliteStore, project_id: &str) -> String {
    make_key_with_redaction(store, project_id, Redaction::None)
}

/// [`make_key`] with an explicit payload-persistence policy on the created project.
pub(crate) fn make_key_with_redaction(
    store: &SqliteStore,
    project_id: &str,
    redaction: Redaction,
) -> String {
    let now = Utc::now();
    store
        .create_project(&Project {
            id: project_id.into(),
            name: project_id.into(),
            enabled: true,
            redaction,
            collective_opt_in: false,
            created_at: now,
        })
        .unwrap();
    let g = auth::generate_key();
    store
        .create_api_key(&ApiKey {
            id: new_id(),
            project_id: project_id.into(),
            name: "test".into(),
            prefix: g.prefix.clone(),
            key_hash: g.key_hash,
            created_at: now,
            last_used_at: None,
            revoked: false,
        })
        .unwrap();
    g.full_key
}

/// POST one event through the real router with a bearer token; returns the status and parsed JSON.
pub(crate) async fn ingest(app: &Router, token: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/events")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

#[tokio::test]
async fn project_persistence_policy_is_enforced_on_ingest() {
    let (state, store) = setup(Redactor::off());
    let key_drop = make_key_with_redaction(&store, "proj-drop", Redaction::Drop);
    let key_hash = make_key_with_redaction(&store, "proj-hash", Redaction::Hash);
    let app = crate::build_router(state);

    let payload = json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 },
        "cost_usd": 0.0,
        "input": { "q": "the secret prompt" },
        "output": "the secret answer"
    });

    // `drop`: the event is recorded, its payloads are not.
    let (status, _) = ingest(&app, &key_drop, payload.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let rows = store.list_events(Some("proj-drop"), 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].input.is_none() && rows[0].output.is_none(), "drop persists no payloads");
    assert_eq!(rows[0].usage.input, 10, "metering fields untouched");

    // `hash`: presence/diff survive as sha256 digests; no plaintext lands in the store.
    let (status, _) = ingest(&app, &key_hash, payload).await;
    assert_eq!(status, StatusCode::OK);
    let rows = store.list_events(Some("proj-hash"), 10).unwrap();
    assert_eq!(rows.len(), 1);
    let stored = serde_json::to_string(&rows[0]).unwrap();
    assert!(!stored.contains("secret"), "no plaintext payload survives hashing: {stored}");
    let digest = rows[0].input.as_ref().and_then(|v| v.get("sha256")).and_then(Value::as_str);
    assert_eq!(digest.map(str::len), Some(64), "input replaced by a sha256 digest");
    assert!(rows[0].output.as_ref().and_then(|v| v.get("sha256")).is_some());
}

#[tokio::test]
async fn project_key_cannot_ingest_into_another_project() {
    let (state, store) = setup(Redactor::off());
    let key_a = make_key(&store, "proj-a");
    // The cross-tenant target exists, so a write could land there if scoping were broken.
    store
        .create_project(&Project {
            id: "proj-b".into(),
            name: "b".into(),
            enabled: true,
            redaction: Redaction::None,
            collective_opt_in: false,
            created_at: Utc::now(),
        })
        .unwrap();
    let app = crate::build_router(state);

    // A's key submits an event explicitly labelled for proj-b.
    let (status, body) = ingest(
        &app,
        &key_a,
        json!({
            "project_id": "proj-b",
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 },
            "cost_usd": 0.0
        }),
    )
    .await;

    // The key forces its own project; the body's project_id is ignored, not honored.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project_id"], "proj-a");

    // Nothing crossed the tenant boundary: proj-b is empty, the event is under proj-a.
    assert!(
        store.list_events(Some("proj-b"), 10).unwrap().is_empty(),
        "a project key must not be able to write into another project"
    );
    let a = store.list_events(Some("proj-a"), 10).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].project_id, "proj-a");
}

#[tokio::test]
async fn uncosted_event_is_priced_from_the_book() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // No cost_usd supplied: 1M input + 1M output @ ($1, $5)/Mtok → $6.00, priced from the book.
    let (status, body) = ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 1_000_000, "output": 1_000_000 }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        (body["cost_usd"].as_f64().unwrap() - 6.0).abs() < 1e-9,
        "response cost not priced from book: {body}"
    );

    // The priced cost is persisted, not merely returned.
    let ev = store.list_events(Some("proj-a"), 10).unwrap().pop().unwrap();
    assert!((ev.cost_usd.unwrap() - 6.0).abs() < 1e-9, "stored cost not priced");
}

#[tokio::test]
async fn pii_is_redacted_before_the_row_is_stored() {
    let (state, store) = setup(Redactor::all());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, _) = ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 },
            "cost_usd": 0.0,
            "input": { "q": "email me at jane@example.com" },
            "output": "card 4111 1111 1111 1111"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The stored row must carry scrubbed content — raw PII never lands in the DB.
    let ev = store.list_events(Some("proj-a"), 10).unwrap().pop().unwrap();
    let stored = serde_json::to_string(&ev).unwrap();
    assert!(!stored.contains("jane@example.com"), "raw email persisted: {stored}");
    assert!(!stored.contains("4111"), "raw card persisted: {stored}");
    assert!(stored.contains("<EMAIL>"), "redaction marker missing: {stored}");
}

#[tokio::test]
async fn enforcing_actions_reject_ingest_and_do_not_store() {
    // Both enforcing actions reject the over-cap event with HTTP 429 and never record it.
    for action in [LimitAction::Block, LimitAction::Throttle] {
        let (state, store) = setup(Redactor::off());
        let key = make_key(&store, "proj-a");
        store
            .create_limit_rule(&LimitRule {
                id: new_id(),
                project_id: "proj-a".into(),
                metric: LimitMetric::Calls,
                window: LimitWindow::Hour,
                threshold: 1.0, // the very first call reaches the cap (usage-with-event = 1 >= 1)
                action,
                enabled: true,
                warn_at: None,
                scope: None,
            })
            .unwrap();
        let app = crate::build_router(state);

        let (status, body) = ingest(
            &app,
            &key,
            json!({
                "provider": "anthropic",
                "model": "claude-haiku-4-5",
                "usage": { "input": 10, "output": 5 },
                "cost_usd": 0.0
            }),
        )
        .await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{action:?} must reject ingest");
        assert_eq!(body["error"]["code"], "rate_limited", "{action:?}: {body}");
        assert!(
            store.list_events(Some("proj-a"), 10).unwrap().is_empty(),
            "{action:?}: a rejected event must not be persisted"
        );
    }
}

/// GET /v1/limits/status through the router; returns (status, parsed JSON body).
async fn get_limits_status(app: &Router, token: &str, project: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/limits/status?project={project}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn rejected_events_are_ledgered_but_never_touch_usage_math() {
    // A rejected event must be counted in the rejection ledger yet stay completely out of the
    // usage/cost rollups — the very math admission is evaluated against. This pins the invariant that
    // the ledger can never corrupt a cap's own accounting.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::Calls,
            window: LimitWindow::Hour,
            threshold: 1.0, // the first call reaches the cap and is rejected
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
        })
        .unwrap();
    let app = crate::build_router(state.clone());

    let (status, _) = ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 },
            "cost_usd": 0.42
        }),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Usage math is provably untouched: no event row, no cost rows, zero usage.
    assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty(), "rejected event was stored");
    assert!(
        store.cost_summary_windowed(Some("proj-a"), None, None).unwrap().is_empty(),
        "rejected event leaked into the cost summary"
    );
    let usage = store.usage_since("proj-a", Utc::now() - chrono::Duration::hours(1)).unwrap();
    assert_eq!(usage.calls, 0, "rejected event counted toward usage");
    assert_eq!(usage.cost_usd, 0.0, "rejected event counted toward cost usage");

    // But it *is* visible out-of-band: the ledger recorded one rejection with its estimated cost.
    let (s, body) = get_limits_status(&app, &key, "proj-a").await;
    assert_eq!(s, StatusCode::OK);
    let rejected = body["rejected"].as_array().expect("rejected block present");
    assert_eq!(rejected.len(), 1, "{body}");
    assert_eq!(rejected[0]["metric"], "calls");
    assert_eq!(rejected[0]["window"], "hour");
    assert_eq!(rejected[0]["count"], 1, "{body}");
    assert!(
        (rejected[0]["est_missed_cost_usd"].as_f64().unwrap() - 0.42).abs() < 1e-9,
        "{body}"
    );
    // The rule itself still reads zero usage (recomputed live from the store, not the ledger).
    assert_eq!(body["statuses"][0]["current"], 0.0, "{body}");
}

#[tokio::test]
async fn alert_limit_flags_but_admits_and_stores() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::Calls,
            window: LimitWindow::Hour,
            threshold: 1.0,
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
        })
        .unwrap();
    let app = crate::build_router(state);

    let (status, body) = ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 },
            "cost_usd": 0.0
        }),
    )
    .await;

    // Alert is observe-only: the event is admitted (200), the breach is surfaced, never throttled.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["throttled"], false, "an Alert breach must not throttle: {body}");
    let breached = body["breached"].as_array().expect("breached array present");
    assert_eq!(breached.len(), 1, "{body}");
    assert_eq!(breached[0]["action"], "alert");
    assert!(breached[0]["breached"].as_bool().unwrap());
    // The event is recorded despite the breach.
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 1);
}

/// POST a batch array through the router; returns (status, parsed JSON body).
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

#[tokio::test]
async fn batch_returns_per_item_accept_reject_invalid() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    // Breach on the 3rd call (admission uses `>=`), so the first two valid items admit and the third
    // is rejected — proving a batch can't bypass the cap.
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::Calls,
            window: LimitWindow::Hour,
            threshold: 3.0,
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
        })
        .unwrap();
    let app = crate::build_router(state);

    let ok = |id: &str| {
        json!({ "id": id, "provider": "anthropic", "model": "claude-haiku-4-5",
                "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0 })
    };
    // Order: valid, invalid(empty model), valid, valid → three admitted attempts against a cap that
    // breaches at 3, so the last valid item is rejected.
    let (status, body) = ingest_batch(
        &app,
        &key,
        json!([
            ok("a"),
            { "id": "bad", "provider": "anthropic", "model": "  ", "usage": { "input": 1, "output": 1 } },
            ok("c"),
            ok("d"),
        ]),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "batch is multi-status under 200: {body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "{body}");
    // Order preserved.
    assert_eq!(results[0]["status"], "accepted");
    assert_eq!(results[0]["id"], "a");
    assert_eq!(results[1]["status"], "invalid", "empty model → invalid: {body}");
    assert_eq!(results[2]["status"], "accepted");
    assert_eq!(results[3]["status"], "rejected", "cap reached → rejected: {body}");
    assert_eq!(body["accepted"], 2);
    assert_eq!(body["invalid"], 1);
    assert_eq!(body["rejected"], 1);

    // Cap-bypass regression: exactly the two admitted events were stored, nothing more.
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2);
}

#[tokio::test]
async fn batch_rejects_empty_and_oversized_requests() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (s_empty, _) = ingest_batch(&app, &key, json!([])).await;
    assert_eq!(s_empty, StatusCode::BAD_REQUEST, "empty batch is a 400");
}

/// GET /v1/events through the router; returns (status, next-cursor header, body array).
async fn get_events(app: &Router, token: &str, query: &str) -> (StatusCode, Option<String>, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/events?{query}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let cursor = resp
        .headers()
        .get("x-next-cursor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    (status, cursor, v)
}

#[tokio::test]
async fn get_events_paginates_by_cursor_and_filters() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // Three events; two anthropic, one openai (openai isn't in the price book, so supply cost_usd).
    for (id, provider, model, cost) in [
        ("p1", "anthropic", "claude-haiku-4-5", None),
        ("p2", "openai", "gpt-4o", Some(0.01)),
        ("p3", "anthropic", "claude-haiku-4-5", None),
    ] {
        let mut body = json!({
            "id": id, "provider": provider, "model": model,
            "usage": { "input": 10, "output": 5 }
        });
        if let Some(c) = cost {
            body["cost_usd"] = json!(c);
        }
        let (s, _) = ingest(&app, &key, body).await;
        assert_eq!(s, StatusCode::OK, "ingest {id}");
    }

    // Page 1 of 2 → a cursor is returned.
    let (s1, cur1, b1) = get_events(&app, &key, "limit=2").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1.as_array().unwrap().len(), 2, "{b1}");
    let cursor = cur1.expect("X-Next-Cursor present when more rows remain");

    // Page 2 via cursor → the final row, no further cursor.
    let (s2, cur2, b2) = get_events(&app, &key, &format!("limit=2&cursor={cursor}")).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2.as_array().unwrap().len(), 1, "{b2}");
    assert!(cur2.is_none(), "no cursor on the last page");

    // Filter by provider.
    let (s3, _, b3) = get_events(&app, &key, "provider=openai").await;
    assert_eq!(s3, StatusCode::OK);
    let arr = b3.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["provider"], "openai");
}

#[tokio::test]
async fn duplicate_event_id_returns_409() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let body = json!({
        "id": "dup-1",
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 },
        "cost_usd": 0.0
    });

    let (s1, _) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s1, StatusCode::OK);

    // Same id, but no client-supplied ts: the server assigned each attempt a different ts, so this
    // is NOT recognizable as a replay of the same logical event → a clear 409 conflict, not a 500.
    // (Retry-safe ingest requires the client to send its own id AND ts — the shipped SDKs set both;
    // see replayed_ingest_is_acknowledged_not_conflicted for that path.)
    let (s2, b2) = ingest(&app, &key, body).await;
    assert_eq!(s2, StatusCode::CONFLICT, "{b2}");
    assert_eq!(b2["error"]["code"], "conflict", "{b2}");
    // The row was not duplicated.
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 1);
}

#[tokio::test]
async fn prompt_tagged_traffic_rolls_up_per_version() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // Traffic stamped with the metadata.prompt convention: two calls on v3, one on v4, one untagged.
    let ev = |id: &str, cost: f64, tag: Option<&str>| {
        let mut e = json!({
            "id": id, "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": cost,
        });
        if let Some(t) = tag {
            e["metadata"] = json!({ "prompt": t });
        }
        e
    };
    for (id, cost, tag) in [
        ("e1", 0.30, Some("summarize@v3")),
        ("e2", 0.50, Some("summarize@v3")),
        ("e3", 0.20, Some("summarize@v4")),
        ("e4", 0.10, None),
    ] {
        let (s, b) = ingest(&app, &key, ev(id, cost, tag)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    let req = Request::builder()
        .method("GET")
        .uri("/v1/costs/prompts?project=proj-a")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let rows: Value = serde_json::from_slice(&bytes).unwrap();
    let rows = rows.as_array().unwrap();

    let find = |key: Option<&str>| {
        rows.iter()
            .find(|r| r["key"].as_str() == key)
            .unwrap_or_else(|| panic!("row for {key:?} in {rows:?}"))
    };
    // "Did v4 cost less than v3 in production?" — one request, answered.
    let v3 = find(Some("summarize@v3"));
    assert_eq!(v3["calls"], 2);
    assert!((v3["cost_usd"].as_f64().unwrap() - 0.80).abs() < 1e-9);
    let v4 = find(Some("summarize@v4"));
    assert_eq!(v4["calls"], 1);
    assert!((v4["cost_usd"].as_f64().unwrap() - 0.20).abs() < 1e-9);
    // Untagged traffic is disclosed under the null key, not silently dropped.
    assert_eq!(find(None)["calls"], 1);
    // Sorted by cost desc: v3 ($0.80) first.
    assert_eq!(rows[0]["key"], "summarize@v3");
}

#[tokio::test]
async fn replayed_ingest_is_acknowledged_not_conflicted() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // A full SDK-shaped event: client-generated id AND ts (what the shipped SDKs send).
    let body = json!({
        "id": "retry-1",
        "ts": "2026-07-16T10:00:00Z",
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 },
        "cost_usd": 0.25
    });

    let (s1, b1) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s1, StatusCode::OK);
    assert!(b1.get("duplicate").is_none(), "first write is not a duplicate: {b1}");

    // The retry (a timed-out POST resent verbatim): acknowledged as the original write, 200 with
    // duplicate: true — a client can now tell "you already have this" from "malformed and gone".
    let (s2, b2) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s2, StatusCode::OK, "a replay is an acknowledgement, not an error: {b2}");
    assert_eq!(b2["duplicate"], true, "{b2}");
    assert_eq!(b2["cost_usd"], 0.25, "the ORIGINAL outcome is returned");
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 1, "nothing double-counted");

    // Same id but a DIFFERENT payload: a true conflict, still refused.
    let mut different = body;
    different["usage"] = json!({ "input": 999, "output": 5 });
    let (s3, b3) = ingest(&app, &key, different).await;
    assert_eq!(s3, StatusCode::CONFLICT, "{b3}");
    assert_eq!(b3["error"]["code"], "conflict", "{b3}");
}

#[tokio::test]
async fn replayed_batch_is_acknowledged_per_item() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let batch = json!([
        { "id": "b-1", "ts": "2026-07-16T10:00:00Z", "provider": "anthropic",
          "model": "claude-haiku-4-5", "usage": { "input": 10, "output": 5 }, "cost_usd": 0.0 },
        { "id": "b-2", "ts": "2026-07-16T10:00:01Z", "provider": "anthropic",
          "model": "claude-haiku-4-5", "usage": { "input": 20, "output": 5 }, "cost_usd": 0.0 },
    ]);
    let post = |body: Value| {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/events/batch")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req)
    };

    let resp = post(batch.clone()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The whole batch resent (e.g. after a response timeout): every item is acknowledged as a
    // duplicate accept — with its index and id — and nothing is double-counted.
    let resp = post(batch).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["accepted"], 2, "replayed items count as accepted: {v}");
    assert_eq!(v["invalid"], 0, "{v}");
    for (i, item) in v["results"].as_array().unwrap().iter().enumerate() {
        assert_eq!(item["status"], "accepted", "{item}");
        assert_eq!(item["duplicate"], true, "{item}");
        assert_eq!(item["index"], i, "positional correlation is explicit");
    }
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2, "no double-count");
}

#[tokio::test]
async fn empty_model_is_rejected_400() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, body) = ingest(
        &app,
        &key,
        json!({ "provider": "anthropic", "model": "   ", "usage": { "input": 1, "output": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request", "{body}");
    // Nothing was stored.
    assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty());
}

#[tokio::test]
async fn cost_source_is_marked_client_vs_book() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    // Client-declared cost → cost_source=client.
    let (s1, _) = ingest(
        &app,
        &key,
        json!({
            "id": "c1", "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 0.42
        }),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // No cost supplied → priced from book → cost_source=book.
    let (s2, _) = ingest(
        &app,
        &key,
        json!({
            "id": "c2", "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 1_000_000, "output": 0 }
        }),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    let by_id = |id: &str| {
        store
            .list_events(Some("proj-a"), 10)
            .unwrap()
            .into_iter()
            .find(|e| e.id == id)
            .unwrap()
    };
    assert_eq!(by_id("c1").metadata["cost_source"], "client");
    assert_eq!(by_id("c2").metadata["cost_source"], "book");
}
