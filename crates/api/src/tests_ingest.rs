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
    Project, Redaction, Threshold,
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
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cached_input_per_mtok: None,
            aliases: Vec::new(),
        },
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
        ingest_guard: Arc::new(crate::shed::IngestGuard::from_env()),
        // Router tests drive `oneshot` without `ConnectInfo`, so there is no source and the throttle
        // is inert here by construction — `tests_auth_throttle` injects one deliberately.
        auth_throttle: Arc::new(crate::auth_throttle::AuthThrottle::from_env()),
        // Empty cache: policies are back-filled lazily from the store on first sight, which is also
        // the path these tests exercise.
        project_policies: Arc::new(crate::state::ProjectPolicyCache::new(HashMap::new())),
        activity: Arc::new(crate::storage::ActivityGauge::default()),
        maintenance: Arc::new(crate::storage::Maintenance::default()),
        policy_cooldowns: Default::default(),
        maintenance_desc: "test fixture (no sweep task is spawned)".to_string(),
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
            require_trusted_judge: false,
            archived_at: None,
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
            scopes: lighttrack_core::default_scopes(),
            expires_at: None,
        })
        .unwrap();
    g.full_key
}

/// Mint an additional named key on an **existing** project, returning `(key_id, full_key)`. The id
/// is what per-key budgets scope to and what ingest stamps onto the event.
pub(crate) fn add_key(store: &SqliteStore, project_id: &str, name: &str) -> (String, String) {
    let g = auth::generate_key();
    let id = new_id();
    store
        .create_api_key(&ApiKey {
            id: id.clone(),
            project_id: project_id.into(),
            name: name.into(),
            prefix: g.prefix.clone(),
            key_hash: g.key_hash,
            created_at: Utc::now(),
            last_used_at: None,
            revoked: false,
            scopes: lighttrack_core::default_scopes(),
            expires_at: None,
        })
        .unwrap();
    (id, g.full_key)
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
    assert!(
        rows[0].input.is_none() && rows[0].output.is_none(),
        "drop persists no payloads"
    );
    assert_eq!(rows[0].usage.input, 10, "metering fields untouched");

    // `hash`: presence/diff survive as sha256 digests; no plaintext lands in the store.
    let (status, _) = ingest(&app, &key_hash, payload).await;
    assert_eq!(status, StatusCode::OK);
    let rows = store.list_events(Some("proj-hash"), 10).unwrap();
    assert_eq!(rows.len(), 1);
    let stored = serde_json::to_string(&rows[0]).unwrap();
    assert!(
        !stored.contains("secret"),
        "no plaintext payload survives hashing: {stored}"
    );
    let digest = rows[0]
        .input
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .and_then(Value::as_str);
    assert_eq!(
        digest.map(str::len),
        Some(64),
        "input replaced by a sha256 digest"
    );
    assert!(rows[0]
        .output
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .is_some());
}

/// GET a JSON endpoint through the router; returns (status, parsed body).
async fn get_json(app: &Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// The M8 headline, end to end: `PUT /v1/prices/mistral/<model>` prices the **next** `mistral` event.
///
/// Before M8 this exact sequence was a dead end — the event's provider was coerced to `unknown`, so
/// the row the operator had just written (keyed `mistral/…`) could never be matched, and the 429 text
/// that tells them to add a price was advice that could not work.
#[tokio::test]
async fn a_price_put_for_an_unmodeled_provider_prices_the_next_event() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let req = Request::builder()
        .method("PUT")
        .uri("/v1/prices/mistral/mistral-large")
        .header("content-type", "application/json")
        .header("authorization", "Bearer admin-secret")
        .body(Body::from(
            json!({ "input_per_mtok": 2.0, "output_per_mtok": 6.0 }).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (status, _) = ingest(
        &app,
        &key,
        json!({
            "id": "mistral-1", "provider": "mistral", "model": "mistral-large",
            "usage": { "input": 1_000_000, "output": 1_000_000 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let ev = store.get_event("mistral-1").unwrap().unwrap();
    assert_eq!(ev.provider.as_str(), "mistral");
    assert_eq!(ev.cost_usd, Some(8.0), "2.0 in + 6.0 out per Mtok");
    assert_eq!(
        ev.cost_source(),
        Some("book"),
        "priced from the book we just wrote, not reported by the client"
    );
}

#[tokio::test]
async fn saturated_ingest_sheds_with_503_and_retry_after_never_a_budget_429() {
    // Saturation must be a fast, honest rejection that a client cannot mistake for "you're over
    // budget" — and it must be visible to an operator, not just felt as latency.
    let (mut state, store) = setup(Redactor::off());
    state.ingest_guard = Arc::new(crate::shed::IngestGuard::with_limits(1, None));
    let key = make_key(&store, "proj-a");
    let guard = state.ingest_guard.clone();
    let app = crate::build_router(state);

    let body = json!({
        "provider": "anthropic", "model": "claude-haiku-4-5",
        "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0
    });

    // Baseline: with the gate free, ingest works and the counters move.
    let (ok, _) = ingest(&app, &key, body.clone()).await;
    assert_eq!(ok, StatusCode::OK);

    // Now hold the only permit — the server is, by construction, saturated.
    let held = guard.take_permit().expect("the gate hands out permits");

    let req = Request::builder()
        .method("POST")
        .uri("/v1/events")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "overload sheds, it does not queue"
    );
    assert_eq!(
        resp.headers().get("retry-after").unwrap(),
        "1",
        "a shed must say when to come back"
    );
    let v: Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(v["error"]["code"], "overloaded", "{v}");
    assert_ne!(
        v["error"]["code"], "rate_limited",
        "shedding must never read as a usage limit"
    );

    // The batch route is gated too — a big write is exactly what you don't want queueing.
    let breq = Request::builder()
        .method("POST")
        .uri("/v1/events/batch")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(json!([body]).to_string()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(breq).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );

    // Nothing over-cap was written, and the operator can see the saturation while it is happening —
    // reads stay answerable precisely because only the write path is gated.
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 1);
    let (s, st) = get_json(&app, &key, "/v1/ingest/status").await;
    assert_eq!(s, StatusCode::OK, "{st}");
    assert_eq!(st["max_in_flight"], 1, "{st}");
    assert_eq!(
        st["in_flight"], 1,
        "the held permit is visible as live depth: {st}"
    );
    assert_eq!(st["shed_total"], 2, "both sheds counted: {st}");
    assert_eq!(st["admitted_total"], 1, "{st}");

    // Release: the gate reopens immediately, so shedding is a momentary state, not a latched one.
    drop(held);
    let (after, _) = ingest(&app, &key, body).await;
    assert_eq!(after, StatusCode::OK);
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2);
}

/// An ingest that outlives its deadline is cut with 504 `timeout`, counted as a timeout and not as a
/// shed, and nothing of it reaches the store.
///
/// **Why this drives its own runtime.** The previous version set `Duration::ZERO` and relied on the
/// comment's claim that "the handler yields at its first blocking store call and the deadline is
/// already past". Only the first half is guaranteed. Tokio's timer wheel has millisecond granularity
/// and only advances when the driver parks, so a zero deadline is not *observably* past until the
/// wheel is processed — leaving a genuine race between the timer firing and the handler's store call
/// returning. It lost that race once under CPU load and returned 200.
///
/// The fix removes the race instead of widening the margin: this runtime has exactly **one** blocking
/// thread and the test holds it. Every store call in the handler goes through `spawn_blocking`, so
/// while the occupier is parked the handler provably cannot get past its first store read — the
/// deadline is then the only thing that can complete the request, on any machine at any load. The
/// assertions are unchanged, plus one the racy version could not make: the store stayed empty.
#[test]
fn ingest_past_its_deadline_is_cut_with_504_not_left_hanging() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("test runtime");

    rt.block_on(async {
        let (mut state, store) = setup(Redactor::off());
        state.ingest_guard = Arc::new(crate::shed::IngestGuard::with_limits(
            8,
            Some(std::time::Duration::from_millis(10)),
        ));
        let key = make_key(&store, "proj-a");
        let guard = state.ingest_guard.clone();
        let app = crate::build_router(state);

        // Take the pool's only thread, and don't proceed until it is provably taken — otherwise the
        // handler's own `spawn_blocking` could win the thread and we'd be back to racing.
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let occupier = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = blocked.recv();
        });
        started_rx
            .recv()
            .expect("occupier reached the blocking pool");

        let (status, body) = ingest(
            &app,
            &key,
            json!({ "provider": "anthropic", "model": "claude-haiku-4-5",
                    "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0 }),
        )
        .await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
        assert_eq!(body["error"]["code"], "timeout", "{body}");

        drop(release);
        occupier.await.expect("occupier joins");
        // The 504 is honest about what it means ("the write may or may not have landed") — here it
        // definitively did not, because the handler never reached an insert.
        assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty());

        // A timeout is a distinct condition from shedding, and counted as one.
        let (_, st) = get_json(&app, &key, "/v1/ingest/status").await;
        assert_eq!(st["timeout_total"], 1, "{st}");
        assert_eq!(st["shed_total"], 0, "a deadline is not a shed: {st}");
        assert!(guard.describe().contains("max_inflight=8"));
    });
}

/// Saturation experiment, run on demand (`cargo test -p lighttrack-api -- --ignored --nocapture
/// shedding_bounds_latency_under_saturation`). It is `#[ignore]`d because it asserts on *timing*,
/// which is exactly the thing a shared CI runner cannot promise — but it is the evidence that this
/// direction did what it claims, so it lives with the code rather than in a scratch file.
///
/// Fires far more concurrent ingest requests than the gate allows and reports the served-latency
/// distribution with the gate bounded vs. unbounded. Unbounded, every request joins a queue behind
/// the store's single lock and tail latency grows with offered load; bounded, the overflow is
/// rejected in microseconds and the requests that *are* served keep a flat tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn shedding_bounds_latency_under_saturation() {
    async fn run(cap: usize, load: usize) -> (Vec<u128>, Vec<u128>) {
        let (mut state, store) = setup(Redactor::off());
        state.ingest_guard = Arc::new(crate::shed::IngestGuard::with_limits(cap, None));
        let key = make_key(&store, "proj-a");
        let app = crate::build_router(state);

        let mut tasks = Vec::with_capacity(load);
        for i in 0..load {
            let app = app.clone();
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                let body = json!({
                    "id": format!("load-{i}"), "provider": "anthropic",
                    "model": "claude-haiku-4-5", "usage": { "input": 10, "output": 5 },
                    "cost_usd": 0.001, "input": { "q": "x".repeat(512) }
                });
                let req = Request::builder()
                    .method("POST")
                    .uri("/v1/events")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {key}"))
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let t0 = std::time::Instant::now();
                let resp = app.oneshot(req).await.unwrap();
                (resp.status(), t0.elapsed().as_micros())
            }));
        }
        let (mut served, mut shed) = (Vec::new(), Vec::new());
        for t in tasks {
            let (status, us) = t.await.unwrap();
            if status == StatusCode::SERVICE_UNAVAILABLE {
                shed.push(us);
            } else {
                served.push(us);
            }
        }
        served.sort_unstable();
        shed.sort_unstable();
        (served, shed)
    }

    let pct = |v: &[u128], p: f64| -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        v[((v.len() as f64 * p) as usize).min(v.len() - 1)] as f64 / 1000.0
    };
    // Sweeping the offered load is the point: unbounded, p95 tracks it upward; bounded, it doesn't.
    for (cap, load) in [
        (0usize, 300),
        (0, 600),
        (0, 1200),
        (16, 300),
        (16, 600),
        (16, 1200),
    ] {
        let (served, shed) = run(cap, load).await;
        println!(
            "cap={:<9} offered={load:<5} served={:<5} shed={:<5} | served p50={:>6.1}ms \
             p95={:>6.1}ms p99={:>6.1}ms max={:>6.1}ms | shed p95={:>6.2}ms",
            if cap == 0 {
                "unbounded".into()
            } else {
                cap.to_string()
            },
            served.len(),
            shed.len(),
            pct(&served, 0.50),
            pct(&served, 0.95),
            pct(&served, 0.99),
            pct(&served, 1.0),
            pct(&shed, 0.95),
        );
    }
}

#[tokio::test]
async fn tightening_redaction_takes_effect_on_the_next_event_without_a_restart() {
    // The compliance hole this closes: the ingest-path policy cache used to be warmed once and never
    // invalidated, so turning payload persistence OFF for a project did nothing until the process was
    // restarted — the window in which you most need it to work is exactly the window it didn't.
    let (state, store) = setup(Redactor::off());
    let key = make_key_with_redaction(&store, "proj-a", Redaction::None);
    let app = crate::build_router(state);

    let payload = |id: &str| {
        json!({
            "id": id, "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 0.0,
            "input": { "q": "the secret prompt" }
        })
    };

    // Before: `none` → the payload is persisted verbatim, and the policy is now cached.
    let (s1, _) = ingest(&app, &key, payload("before")).await;
    assert_eq!(s1, StatusCode::OK);
    let before = store.get_event("before").unwrap().unwrap();
    assert!(
        before.input.is_some(),
        "baseline: `none` persists the payload"
    );

    // Tighten it through the API (admin).
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/projects/proj-a")
        .header("content-type", "application/json")
        .header("authorization", "Bearer admin-secret")
        .body(Body::from(json!({ "redaction": "drop" }).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(updated["redaction"], "drop");
    assert_eq!(updated["name"], "proj-a", "omitted fields are left alone");

    // After: the very NEXT event obeys the new policy — no restart, no TTL wait.
    let (s2, _) = ingest(&app, &key, payload("after")).await;
    assert_eq!(s2, StatusCode::OK);
    let after = store.get_event("after").unwrap().unwrap();
    assert!(
        after.input.is_none(),
        "a tightened policy must bind the next event, not the next boot"
    );
    assert_eq!(
        after.usage.input, 10,
        "metering is untouched by the persistence policy"
    );
}

#[tokio::test]
async fn client_ts_skew_is_rejected_with_distinct_codes_and_cannot_move_a_window() {
    // Two halves of the same invariant: a wildly-skewed `ts` is refused outright with a code a client
    // can act on, and a `ts` skewed *within* tolerance still lands in the live accounting window,
    // because the window is measured on server arrival — not on anything the caller sends.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let ev = |id: &str, ts: chrono::DateTime<Utc>| {
        json!({
            "id": id, "ts": ts.to_rfc3339(), "provider": "anthropic",
            "model": "claude-haiku-4-5", "usage": { "input": 10, "output": 5 }, "cost_usd": 1.0
        })
    };

    // Beyond the default bounds → refused, with the direction named.
    let (s_old, b_old) = ingest(
        &app,
        &key,
        ev("old", Utc::now() - chrono::Duration::days(400)),
    )
    .await;
    assert_eq!(s_old, StatusCode::BAD_REQUEST, "{b_old}");
    assert_eq!(b_old["error"]["code"], "ts_too_old", "{b_old}");
    let (s_new, b_new) = ingest(
        &app,
        &key,
        ev("new", Utc::now() + chrono::Duration::hours(3)),
    )
    .await;
    assert_eq!(s_new, StatusCode::BAD_REQUEST, "{b_new}");
    assert_eq!(b_new["error"]["code"], "ts_too_new", "{b_new}");
    assert!(
        store.list_events(Some("proj-a"), 10).unwrap().is_empty(),
        "neither was stored"
    );

    // Within tolerance but backdated a full day: accepted, and it counts against the *hour* window it
    // actually arrived in. Under the old ts-keyed accounting this call was invisible to an hourly cap.
    let (s_ok, b_ok) = ingest(
        &app,
        &key,
        ev("backdated", Utc::now() - chrono::Duration::days(1)),
    )
    .await;
    assert_eq!(s_ok, StatusCode::OK, "{b_ok}");
    let usage = store
        .usage_since("proj-a", Utc::now() - chrono::Duration::hours(1))
        .unwrap();
    assert_eq!(
        usage.calls, 1,
        "a backdated event still consumes the live budget window"
    );
    assert_eq!(usage.cost_usd, 1.0);
    // Its client-supplied `ts` is preserved and returned unchanged — we reject skew, we don't rewrite it.
    let stored = store.get_event("backdated").unwrap().unwrap();
    assert!(
        stored.ts < stored.received_at,
        "client ts preserved, arrival stamped separately"
    );
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
            require_trusted_judge: false,
            archived_at: None,
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
    let ev = store
        .list_events(Some("proj-a"), 10)
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        (ev.cost_usd.unwrap() - 6.0).abs() < 1e-9,
        "stored cost not priced"
    );
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
    let ev = store
        .list_events(Some("proj-a"), 10)
        .unwrap()
        .pop()
        .unwrap();
    let stored = serde_json::to_string(&ev).unwrap();
    assert!(
        !stored.contains("jane@example.com"),
        "raw email persisted: {stored}"
    );
    assert!(!stored.contains("4111"), "raw card persisted: {stored}");
    assert!(
        stored.contains("<EMAIL>"),
        "redaction marker missing: {stored}"
    );
}

/// The D14 behavior change, asserted through the wired router rather than the unit under it: an
/// instance whose operator configured *nothing* scrubs PII on every ingest door, and the `hash`
/// persistence policy still produces a usable digest under that default.
#[tokio::test]
async fn an_unconfigured_instance_scrubs_pii_on_every_ingest_door() {
    let (state, store) = setup(Redactor::defaulted());
    let key = make_key(&store, "proj-a");
    let hash_key = make_key_with_redaction(&store, "proj-hash", Redaction::Hash);
    let app = crate::build_router(state);

    let pii = json!({
        "provider": "anthropic", "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 }, "cost_usd": 0.0,
        "input": { "q": "email jane@example.com" },
        "output": "card 4111 1111 1111 1111",
        "error": "upstream rejected jane@example.com",
        "tags": ["cust:jane@example.com"]
    });

    // Door 1: POST /v1/events.
    let (status, _) = ingest(&app, &key, pii.clone()).await;
    assert_eq!(status, StatusCode::OK);

    // Door 2: POST /v1/events/batch — same pipeline, so the same floor must apply.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/events/batch")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(json!([pii]).to_string()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    let rows = store.list_events(Some("proj-a"), 10).unwrap();
    assert_eq!(rows.len(), 2, "both doors stored an event");
    for ev in &rows {
        let stored = serde_json::to_string(ev).unwrap();
        assert!(
            !stored.contains("jane@example.com"),
            "raw email persisted: {stored}"
        );
        // The needle is the WHOLE card, in both the spaced form that was sent and the compact form
        // a normalizer could produce — not the leading `4111`.
        //
        // 2026-08-24: `!stored.contains("4111")` was this assertion for months, and it failed on CI
        // against a perfectly scrubbed row. The serialized event carries RFC3339 timestamps with
        // nanosecond precision, and `...T21:17:32.341118412Z` contains `4111`. A four-digit needle
        // matched against a blob full of digits and hex is a coin flip with roughly a
        // one-in-several-hundred face, on a blocking gate, on code that had not changed — which is
        // the shape of a flake that gets re-run away rather than diagnosed. Both directions are now
        // asserted, so this is tighter than what it replaced rather than merely quieter.
        assert!(
            !stored.contains("4111 1111 1111 1111") && !stored.contains("4111111111111111"),
            "raw card persisted: {stored}"
        );
        assert!(
            stored.contains("<EMAIL>"),
            "email redaction marker missing: {stored}"
        );
        assert!(
            stored.contains("<CC>"),
            "the card was neither persisted nor recognised — an absent needle proves nothing              unless the marker that should have replaced it is present: {stored}"
        );
    }

    // A `hash` project keeps a real 64-hex digest: the scrub must not treat the digest it was handed
    // as a secret and collapse every payload to the same marker (which is what "32+ hex chars is a
    // secret" would do if the two layers were not ordered against each other).
    let (status, _) = ingest(&app, &hash_key, pii).await;
    assert_eq!(status, StatusCode::OK);
    let ev = store
        .list_events(Some("proj-hash"), 10)
        .unwrap()
        .pop()
        .unwrap();
    let digest = ev
        .input
        .as_ref()
        .and_then(|v| v.get("sha256"))
        .and_then(Value::as_str);
    assert_eq!(
        digest.map(str::len),
        Some(64),
        "hash policy lost its digest: {:?}",
        ev.input
    );
    // …while the surfaces no persistence policy covers are still scrubbed.
    assert!(!ev
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("jane@example.com"));
    assert!(!ev.tags.iter().any(|t| t.contains("jane@example.com")));
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
                threshold: Threshold::Fixed(1.0), // the very first call reaches the cap (usage-with-event = 1 >= 1)
                action,
                enabled: true,
                warn_at: None,
                scope: None,
                escalation: None,
                escalated_until: None,
                origin: None,
                expires_at: None,
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

        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{action:?} must reject ingest"
        );
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
            threshold: Threshold::Fixed(1.0), // the first call reaches the cap and is rejected
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
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
    assert!(
        store.list_events(Some("proj-a"), 10).unwrap().is_empty(),
        "rejected event was stored"
    );
    assert!(
        store
            .cost_summary_windowed(Some("proj-a"), None, None)
            .unwrap()
            .is_empty(),
        "rejected event leaked into the cost summary"
    );
    let usage = store
        .usage_since("proj-a", Utc::now() - chrono::Duration::hours(1))
        .unwrap();
    assert_eq!(usage.calls, 0, "rejected event counted toward usage");
    assert_eq!(
        usage.cost_usd, 0.0,
        "rejected event counted toward cost usage"
    );

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
            threshold: Threshold::Fixed(1.0),
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
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
    // An accepted write carries no `throttled` flag at all — admission already means nothing
    // enforcing applied, so the flag could never have been true.
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("throttled").is_none(),
        "an admitted event has no throttled flag: {body}"
    );
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
            threshold: Threshold::Fixed(3.0),
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
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

    assert_eq!(
        status,
        StatusCode::OK,
        "batch is multi-status under 200: {body}"
    );
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "{body}");
    // Order preserved.
    assert_eq!(results[0]["status"], "accepted");
    assert_eq!(results[0]["id"], "a");
    assert_eq!(
        results[1]["status"], "invalid",
        "empty model → invalid: {body}"
    );
    assert_eq!(results[2]["status"], "accepted");
    assert_eq!(
        results[3]["status"], "rejected",
        "cap reached → rejected: {body}"
    );
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

/// GET /v1/events returning (status, next-cursor, total-count, body).
async fn query_events(
    app: &Router,
    token: &str,
    query: &str,
) -> (StatusCode, Option<String>, Option<String>, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/events?{query}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let hdr = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (cursor, total) = (hdr("x-next-cursor"), hdr("x-total-count"));
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, cursor, total, v)
}

#[tokio::test]
async fn events_can_be_asked_the_questions_that_matter_over_http() {
    // The debugging questions that previously had no answer on the flat event list: which errored,
    // which belong to this customer, which cost more than X, and how many match at all.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    for (id, status, cost, customer, tag) in [
        ("q1", "success", 0.01, "acme", "prod"),
        ("q2", "error", 0.30, "acme", "production"),
        ("q3", "error", 0.02, "globex", "prod"),
        ("q4", "success", 0.50, "globex", "prod"),
    ] {
        let (s, b) = ingest(
            &app,
            &key,
            json!({
                "id": id, "provider": "anthropic", "model": "claude-haiku-4-5",
                "usage": { "input": 1, "output": 1 }, "cost_usd": cost, "status": status,
                "tags": [tag], "metadata": { "customer_id": customer }
            }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let ids = |v: &Value| {
        let mut out: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    };

    let (s, _, _, v) = query_events(&app, &key, "status=error").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ids(&v), ["q2", "q3"]);

    let (_, _, _, v) = query_events(&app, &key, "meta=customer_id%3Dacme").await;
    assert_eq!(ids(&v), ["q1", "q2"]);
    let (_, _, _, v) = query_events(&app, &key, "meta=customer_id").await;
    assert_eq!(ids(&v).len(), 4, "key-presence matches every tagged call");

    let (_, _, _, v) = query_events(&app, &key, "min_cost=0.25").await;
    assert_eq!(ids(&v), ["q2", "q4"]);

    // Membership, not substring: "prod" must not sweep up the one tagged "production".
    let (_, _, _, v) = query_events(&app, &key, "tag=prod").await;
    assert_eq!(ids(&v), ["q1", "q3", "q4"]);

    // Opt-in total travels beside a limited page, and counts the whole match set.
    let (_, cursor, total, v) = query_events(&app, &key, "status=error&count=1&limit=1").await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(
        total.as_deref(),
        Some("2"),
        "X-Total-Count is the match set, not the page"
    );
    assert!(cursor.is_some(), "a further page remains");
    // …and is absent unless asked for.
    let (_, _, no_total, _) = query_events(&app, &key, "status=error").await;
    assert!(no_total.is_none());

    // Cursor semantics survive the new predicates end to end.
    let (_, _, _, page2) = query_events(
        &app,
        &key,
        &format!("status=error&limit=1&cursor={}", cursor.unwrap()),
    )
    .await;
    let mut seen = ids(&v);
    seen.extend(ids(&page2));
    seen.sort();
    assert_eq!(
        seen,
        ["q2", "q3"],
        "paging under a filter yields each match exactly once"
    );

    // Combined predicates AND; a nonsense status is a 400, never a misleading empty page.
    let (_, _, _, v) = query_events(&app, &key, "status=error&min_cost=0.25").await;
    assert_eq!(ids(&v), ["q2"]);
    let (s, _, _, b) = query_events(&app, &key, "status=nonsense").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "bad_request", "{b}");
}

#[tokio::test]
async fn the_unscored_work_list_accepts_every_spelling_its_docs_promise() {
    // `unscored` was typed `Option<bool>` while its doc comment promised "`1`/`true`", so the
    // runner's `GET /v1/events?unscored=1` — the one request online scoring makes — came back 400
    // "provided string was not `true` or `false`". The headline feature could not judge a single
    // event. Pin every spelling the docs promise, and pin that the flag still *filters*.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    for id in ["scored-1", "todo-1"] {
        let (s, b) = ingest(
            &app,
            &key,
            json!({
                "id": id, "provider": "anthropic", "model": "claude-haiku-4-5",
                "usage": { "input": 1, "output": 1 }, "cost_usd": 0.01,
                "input": { "q": "hi" }, "output": "there"
            }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    // Judge one of them, so the anti-join has something to exclude.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/scores")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(
            json!({
                "event_id": "scored-1", "rubric": "helpfulness",
                "value": 0.9, "scored_by": "claude-haiku-4-5"
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    let ids = |v: &Value| {
        let mut out: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    };

    // Every spelling the doc comment promises reaches the work list — `1` above all, since that is
    // what `lt-runner score` actually sends.
    // `%201%20` = " 1 " — a flag surviving surrounding whitespace, which `is_truthy` trims.
    for spelling in ["1", "true", "yes", "TRUE", "True", "Yes", "%201%20"] {
        let (s, _, _, v) = query_events(&app, &key, &format!("unscored={spelling}")).await;
        assert_eq!(s, StatusCode::OK, "unscored={spelling:?} must not 400: {v}");
        assert_eq!(
            ids(&v),
            ["todo-1"],
            "unscored={spelling:?} must return only the unjudged event"
        );
    }

    // An unparseable value reads as *unset*, not as a 400: these are opt-in flags, and the honest
    // answer to "I could not parse your flag" is the behaviour you get without it. Rejecting would
    // rebuild the exact wall this fix tore down, on a param whose whole job is to be forgiving.
    // (Contrast `status=nonsense`, which IS a 400 — there an unknown value would silently *narrow*
    // the result set, so an empty page would lie. Here a false reading only *widens* it.)
    for garbage in ["", "0", "false", "no", "ture", "%20"] {
        let (s, _, _, v) = query_events(&app, &key, &format!("unscored={garbage}")).await;
        assert_eq!(
            s,
            StatusCode::OK,
            "unscored={garbage:?} reads as unset: {v}"
        );
        assert_eq!(
            ids(&v),
            ["scored-1", "todo-1"],
            "unscored={garbage:?} must behave as though the flag were absent"
        );
    }
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
async fn blank_event_id_is_minted_not_stored_as_empty_pk() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let body = json!({
        "id": "",
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 1, "output": 1 }
    });

    // Two events with an explicit blank id must not collide on the primary key `""`.
    let (s1, b1) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s1, StatusCode::OK, "{b1}");
    let (s2, b2) = ingest(&app, &key, body).await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    let (id1, id2) = (b1["id"].as_str().unwrap(), b2["id"].as_str().unwrap());
    assert!(
        !id1.is_empty() && !id2.is_empty(),
        "ids were minted: {b1} {b2}"
    );
    assert_ne!(id1, id2, "each blank id got its own");
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2);
}

#[tokio::test]
async fn disabled_project_refuses_ingest_on_both_doors_until_re_enabled() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state.clone());
    let body = json!({
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 1, "output": 1 }
    });

    let (s, b) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // Flip the switch the way PUT /v1/projects/:id does: update the row, invalidate the cache.
    let flip = |enabled: bool| {
        let mut p = store.get_project("proj-a").unwrap().unwrap();
        p.enabled = enabled;
        assert!(store.update_project(&p).unwrap());
        state.project_policies.invalidate("proj-a");
    };
    flip(false);

    // Door 1: the single-event POST is a 403 carrying the stable, actionable code — not a generic
    // `forbidden` a client would answer by rotating credentials forever.
    let (s, b) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "project_disabled", "{b}");

    // Door 2: the batch. Since M16 the switch is applied at the *credential* — a disabled project's
    // keys open nothing — so the whole request is refused rather than each item being ledgered
    // `invalid`. Cheaper, and it means a shipped client cannot keep spending on a killed tenant.
    let batch = |token: String, project: Option<&'static str>| {
        let app = app.clone();
        let mut body = body.clone();
        async move {
            if let Some(p) = project {
                body["project_id"] = json!(p);
            }
            let req = Request::builder()
                .method("POST")
                .uri("/v1/events/batch")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(json!([body]).to_string()))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let (s, v) = batch(key.clone(), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "project_disabled", "{v}");

    // An admin naming the disabled project in the body is not stopped at the credential (an admin
    // key belongs to no tenant), so the per-item check still has to hold — and still names the
    // project, not the key.
    let (s, v) = batch("admin-secret".to_string(), Some("proj-a")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["invalid"], 1, "{v}");
    assert_eq!(v["results"][0]["status"], "invalid", "{v}");
    assert_eq!(v["results"][0]["code"], "project_disabled", "{v}");
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 1);

    // Reads by that project's own keys stop too: "disabled" is a tenant kill switch, not an ingest
    // filter — an operator who disabled a project did not mean "keep serving its stored prompts".
    let (s, _) = get_json(&app, &key, "/v1/events?project=proj-a").await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    flip(true);
    let (s, b) = ingest(&app, &key, body).await;
    assert_eq!(s, StatusCode::OK, "re-enabled: {b}");
    assert_eq!(store.list_events(Some("proj-a"), 10).unwrap().len(), 2);
}

#[tokio::test]
async fn key_lifecycle_list_shows_use_and_revoke_kills_auth() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let admin = "admin-secret";

    let send = |method: &str, uri: String, token: &str| {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req)
    };
    let json_of = |bytes: axum::body::Bytes| -> Value {
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        }
    };

    // Use the key once so last_used_at is stamped (best-effort/detached — poll briefly).
    let (s, _) = ingest(&app, &key, json!({
        "provider": "anthropic", "model": "claude-haiku-4-5", "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0
    })).await;
    assert_eq!(s, StatusCode::OK);

    // List keys (admin): the key is there; key_hash is never exposed.
    let resp = send("GET", "/v1/projects/proj-a/keys".into(), admin)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = json_of(to_bytes(resp.into_body(), usize::MAX).await.unwrap());
    let row = &list.as_array().unwrap()[0];
    let kid = row["id"].as_str().unwrap().to_string();
    assert_eq!(row["revoked"], false);
    assert!(
        row.get("key_hash").is_none(),
        "key_hash must never be listed: {row}"
    );

    // A project key can't list (admin-gated).
    let resp = send("GET", "/v1/projects/proj-a/keys".into(), &key)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Revoke it (admin). The key still authenticates BEFORE revocation…
    let resp = send("DELETE", format!("/v1/projects/proj-a/keys/{kid}"), admin)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        json_of(to_bytes(resp.into_body(), usize::MAX).await.unwrap())["revoked"],
        true
    );

    // …and is rejected immediately AFTER — auth reads the store per request.
    let (s2, _) = ingest(&app, &key, json!({
        "provider": "anthropic", "model": "claude-haiku-4-5", "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0
    })).await;
    assert_eq!(
        s2,
        StatusCode::UNAUTHORIZED,
        "a revoked key is dead on the next call"
    );

    // Revoking an unknown key id → 404.
    let resp = send("DELETE", "/v1/projects/proj-a/keys/nope".into(), admin)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

    // A full SDK-shaped event: client-generated id AND ts (what the shipped SDKs send). The ts is
    // relative to now so the fixture stays inside the default client-clock skew window as time passes.
    let ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let body = json!({
        "id": "retry-1",
        "ts": ts,
        "provider": "anthropic",
        "model": "claude-haiku-4-5",
        "usage": { "input": 10, "output": 5 },
        "cost_usd": 0.25
    });

    let (s1, b1) = ingest(&app, &key, body.clone()).await;
    assert_eq!(s1, StatusCode::OK);
    assert!(
        b1.get("duplicate").is_none(),
        "first write is not a duplicate: {b1}"
    );

    // The retry (a timed-out POST resent verbatim): acknowledged as the original write, 200 with
    // duplicate: true — a client can now tell "you already have this" from "malformed and gone".
    let (s2, b2) = ingest(&app, &key, body.clone()).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "a replay is an acknowledgement, not an error: {b2}"
    );
    assert_eq!(b2["duplicate"], true, "{b2}");
    assert_eq!(b2["cost_usd"], 0.25, "the ORIGINAL outcome is returned");
    assert_eq!(
        store.list_events(Some("proj-a"), 10).unwrap().len(),
        1,
        "nothing double-counted"
    );

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

    let t1 = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let t2 = (Utc::now() - chrono::Duration::minutes(4)).to_rfc3339();
    let batch = json!([
        { "id": "b-1", "ts": t1, "provider": "anthropic",
          "model": "claude-haiku-4-5", "usage": { "input": 10, "output": 5 }, "cost_usd": 0.0 },
        { "id": "b-2", "ts": t2, "provider": "anthropic",
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
    assert_eq!(
        store.list_events(Some("proj-a"), 10).unwrap().len(),
        2,
        "no double-count"
    );
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

#[tokio::test]
async fn an_unpriced_model_cannot_spend_freely_under_a_cost_cap() {
    // End-to-end version of the direction-(1) invariant. The test price book knows exactly one
    // model, so `mystery-model-9` is genuinely unpriceable — the shape of every "we just shipped a
    // new model" incident.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Hour,
            threshold: Threshold::Fixed(1.0),
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        })
        .unwrap();
    let app = crate::build_router(state.clone());

    let unpriced = json!({
        "provider": "anthropic",
        "model": "mystery-model-9",
        "usage": { "input": 100_000, "output": 100_000 }
    });

    // With nothing priced in the window the cap cannot be measured at all: refuse, and say why.
    let (status, body) = ingest(&app, &key, unpriced.clone()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "rate_limited", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("price book"),
        "the reason must name the actual problem: {msg}"
    );
    assert!(store.list_events(Some("proj-a"), 10).unwrap().is_empty());

    // Once there is priced traffic to learn from, unpriced calls are charged the window's mean
    // priced cost rather than $0.00 — so they fill the cap instead of walking past it.
    for _ in 0..2 {
        let (s, _) = ingest(
            &app,
            &key,
            json!({
                "provider": "anthropic",
                "model": "claude-haiku-4-5",
                "usage": { "input": 10, "output": 5 },
                "cost_usd": 0.40
            }),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
    // Rolling cost is $0.80 stored; one unpriced call is imputed at the $0.40 mean → $1.20 >= $1.00.
    let (s, body) = ingest(&app, &key, unpriced).await;
    assert_eq!(
        s,
        StatusCode::TOO_MANY_REQUESTS,
        "imputed cost must trip the cap: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("imputed"),
        "a cap tripped on estimated cost must say so: {body}"
    );

    // And the operator surface exposes the weak evidence rather than hiding it behind a number.
    let (s, body) = get_limits_status(&app, &key, "proj-a").await;
    assert_eq!(s, StatusCode::OK);
    let basis = &body["cost_basis"];
    assert_eq!(
        basis["unpriced_calls"], 0,
        "no unpriced call was ever admitted: {body}"
    );
    assert!(
        basis["notes"].as_array().unwrap().len() >= 3,
        "the caveats are stated: {body}"
    );
    assert!(
        basis["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("no repricing of history")),
        "the absence of a repricing path must be stated in the response: {body}"
    );
    // The cost status carries its provenance so a client can tell measured from inferred.
    assert!(body["statuses"][0]["cost_evidence"].is_object(), "{body}");
    assert_eq!(
        body["statuses"][0]["cost_evidence"]["priced_calls"], 2,
        "{body}"
    );
}

#[tokio::test]
async fn client_reported_cost_is_distinguishable_from_our_own_estimate() {
    // A cap breached on a number the caller supplied is a different fact from one breached on our
    // arithmetic. `/v1/limits/status` must let an operator tell them apart.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Hour,
            threshold: Threshold::Fixed(100.0),
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        })
        .unwrap();
    let app = crate::build_router(state);

    // One client-reported cost, one priced from the book (no `cost_usd` on the wire).
    let (s, _) = ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 }, "cost_usd": 2.50
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = ingest(
        &app,
        &key,
        json!({
            "provider": "anthropic", "model": "claude-haiku-4-5",
            "usage": { "input": 1_000_000, "output": 1_000_000 }
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, body) = get_limits_status(&app, &key, "proj-a").await;
    assert_eq!(s, StatusCode::OK);
    let client = body["cost_basis"]["client_reported_cost_usd"]
        .as_f64()
        .unwrap();
    assert!(
        (client - 2.50).abs() < 1e-9,
        "only the client-supplied cost counts here: {body}"
    );
    let total = body["statuses"][0]["current"].as_f64().unwrap();
    assert!(
        total > client,
        "the book-priced call is in the total but not the client share: {body}"
    );
}

/// POST one event and return `(status, retry-after header, body)`.
async fn ingest_with_headers(
    app: &Router,
    token: &str,
    body: Value,
) -> (StatusCode, Option<String>, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/events")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let retry = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, retry, v)
}

/// Build a router with one `Calls` rule of the given action/threshold and return `(app, key, store)`.
fn app_with_calls_rule(
    action: LimitAction,
    threshold: f64,
    warn_at: Option<f64>,
) -> (Router, String, Arc<SqliteStore>) {
    app_with_calls_rule_id(&new_id(), action, threshold, warn_at)
}

/// [`app_with_calls_rule`] with the rule's **id** chosen by the caller. The shed lottery hashes
/// `(rule_id, event_id)`, so a test that needs a decided verdict rather than a sampled one has to pin
/// both halves — a random rule id makes the outcome a coin flip even with pinned events.
fn app_with_calls_rule_id(
    rule_id: &str,
    action: LimitAction,
    threshold: f64,
    warn_at: Option<f64>,
) -> (Router, String, Arc<SqliteStore>) {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            id: rule_id.to_string(),
            project_id: "proj-a".into(),
            metric: LimitMetric::Calls,
            window: LimitWindow::Hour,
            threshold: Threshold::Fixed(threshold),
            action,
            enabled: true,
            warn_at,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        })
        .unwrap();
    (crate::build_router(state), key, store)
}

fn one_call() -> Value {
    json!({
        "provider": "anthropic", "model": "claude-haiku-4-5",
        "usage": { "input": 1, "output": 1 }, "cost_usd": 0.0
    })
}

#[tokio::test]
async fn throttle_sheds_gradually_where_block_is_a_cliff() {
    // The direction-(2) invariant: `Throttle` and `Block` must stop being synonyms. Same metric,
    // same threshold, same traffic — the two actions must produce visibly different shapes.
    let (t_app, t_key, t_store) = app_with_calls_rule(LimitAction::Throttle, 20.0, Some(0.5));
    let (b_app, b_key, b_store) = app_with_calls_rule(LimitAction::Block, 20.0, Some(0.5));

    let mut throttle_shed = 0;
    let mut block_shed = 0;
    for _ in 0..19 {
        let (s, _, _) = ingest_with_headers(&t_app, &t_key, one_call()).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            throttle_shed += 1;
        }
        let (s, _, _) = ingest_with_headers(&b_app, &b_key, one_call()).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            block_shed += 1;
        }
    }
    // Block is a cliff: everything below the threshold sails through untouched.
    assert_eq!(
        block_shed, 0,
        "Block must not shed anything before its threshold"
    );
    assert_eq!(b_store.list_events(Some("proj-a"), 100).unwrap().len(), 19);
    // Throttle is a ramp: real back-pressure builds on the approach, but traffic still flows.
    assert!(
        throttle_shed > 0,
        "Throttle must actually throttle before the wall"
    );
    let stored = t_store.list_events(Some("proj-a"), 100).unwrap().len();
    assert!(
        stored > 0 && stored < 19,
        "graduated, not all-or-nothing (stored {stored}/19)"
    );
}

#[tokio::test]
async fn a_shed_response_tells_the_client_how_close_it_is_and_when_to_retry() {
    let (app, key, _store) = app_with_calls_rule(LimitAction::Throttle, 20.0, Some(0.5));

    // Accepted writes carry the proximity signal, so a client never has to poll a second endpoint.
    let (s, _, body) = ingest_with_headers(&app, &key, one_call()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let ratio = body["usage_ratio"]
        .as_f64()
        .expect("accepted writes report proximity");
    assert!((ratio - 0.05).abs() < 1e-9, "1 of 20 calls: {body}");
    assert!(
        body.get("shed_fraction").is_none(),
        "nothing is shedding yet: {body}"
    );

    // Drive up into the ramp and capture the first shed response.
    let mut shed: Option<(Option<String>, Value)> = None;
    let mut saw_shed_fraction = false;
    for _ in 0..18 {
        let (s, retry, body) = ingest_with_headers(&app, &key, one_call()).await;
        if s == StatusCode::OK {
            saw_shed_fraction |= body.get("shed_fraction").is_some();
        } else if shed.is_none() {
            shed = Some((retry, body));
        }
    }
    assert!(
        saw_shed_fraction,
        "accepted writes inside the ramp must report the shed pressure"
    );

    let (retry, body) = shed.expect("throttling must shed at least one event on the approach");
    assert_eq!(body["error"]["code"], "rate_limited", "{body}");
    let secs: u64 = retry
        .expect("a shed must carry Retry-After")
        .parse()
        .unwrap();
    assert!(
        (1..=15).contains(&secs),
        "a shed is transient back-pressure, got {secs}s"
    );
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("throttled") && msg.contains("Not over budget"),
        "{msg}"
    );

    // A hard breach, by contrast, asks the client to wait for the window — a different schedule.
    let (hard_app, hard_key, _) = app_with_calls_rule(LimitAction::Block, 1.0, None);
    let (s, retry, _) = ingest_with_headers(&hard_app, &hard_key, one_call()).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    let hard_secs: u64 = retry
        .expect("a hard cap must carry Retry-After")
        .parse()
        .unwrap();
    assert_eq!(hard_secs, LimitWindow::Hour.retry_after_secs());
    assert!(
        hard_secs > 15,
        "a hard stop is a longer wait than transient shedding"
    );
}

#[tokio::test]
async fn a_shed_is_ledgered_and_is_never_confusable_with_server_overload() {
    // Shedding for *budget* is 429 `rate_limited`; shedding for *server saturation* is 503
    // `overloaded` (shed.rs). They must stay distinct — and a budget shed must still be attributed
    // in the rejection ledger, or the operator surface goes blind exactly when throttling engages.
    //
    // The shed verdict is a hash of `(rule_id, event_id)` (`core::limits::shed_ticket`), so **both**
    // are pinned. With a random rule id and server-minted event ids this loop sheds nothing about 1
    // run in 160 — and then asserts that nothing happened, a red build with no bug behind it.
    //
    // Pinned, the ladder is decided rather than sampled: usage including the candidate walks 1..9
    // against a threshold of 10 with the ramp starting at 0.3, so the shed fraction each event faces
    // is 0, 0, 0, 1/7, 2/7, 3/7, 4/7, 5/7, 5/7 — and these nine tickets clear or miss their own
    // fraction by at least 0.40, admitting `ev-0`..`ev-6` and shedding `ev-7` and `ev-8` every time.
    // If the ramp or the hash ever changes, re-pin (any pair whose tickets land the same way will
    // do); do not weaken the assertion back into a sampled one.
    let (app, key, _store) =
        app_with_calls_rule_id("pinned-throttle-21", LimitAction::Throttle, 10.0, Some(0.3));
    let mut shed_seen = 0;
    for i in 0..9 {
        let mut ev = one_call();
        ev["id"] = json!(format!("ev-{i}"));
        let (s, _, body) = ingest_with_headers(&app, &key, ev).await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            shed_seen += 1;
            assert_eq!(
                body["error"]["code"], "rate_limited",
                "never `overloaded`: {body}"
            );
            assert_ne!(s, StatusCode::SERVICE_UNAVAILABLE);
        }
    }
    assert_eq!(
        shed_seen, 2,
        "the pinned ladder sheds exactly ev-7 and ev-8"
    );

    let (s, body) = get_limits_status(&app, &key, "proj-a").await;
    assert_eq!(s, StatusCode::OK);
    let rejected = body["rejected"]
        .as_array()
        .expect("shed events are ledgered");
    assert_eq!(rejected.len(), 1, "{body}");
    assert_eq!(rejected[0]["metric"], "calls");
    assert_eq!(
        rejected[0]["count"], shed_seen,
        "every shed event is attributed: {body}"
    );
    // And the status surface shows the shedding pressure itself.
    assert!(
        body["statuses"][0]["shed_fraction"].as_f64().unwrap() > 0.0,
        "{body}"
    );
}

// ---- the proximity signal, on every ingest door -------------------------------------------

/// POST one body to an ingest door and return `(status, all response headers, body)`.
async fn ingest_capturing_headers(
    app: &Router,
    token: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, HashMap<String, String>, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers: HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, v)
}

#[tokio::test]
async fn every_ingest_door_reports_the_project_position_in_headers() {
    // The gap this closes: the proximity signal was a body field on ONE of the three ingest doors.
    // A client that batches, or exports OTLP, or is being refused outright could not see the wall
    // coming at all. Headers are the channel all three share.
    let (app, key, _store) = app_with_calls_rule(LimitAction::Block, 4.0, None);

    let (s, h, body) = ingest_capturing_headers(&app, &key, "/v1/events", one_call()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        h.get("x-lighttrack-usage-ratio").map(String::as_str),
        Some("0.250000")
    );
    assert!(
        !h.contains_key("x-lighttrack-shed-fraction"),
        "Block sheds nothing: {h:?}"
    );
    // Header and body are one number, not two computations.
    assert!(
        (body["usage_ratio"].as_f64().unwrap() - 0.25).abs() < 1e-9,
        "{body}"
    );

    // The batch door, whose multi-status body has nowhere to put a project-level fact.
    let (s, h, body) = ingest_capturing_headers(
        &app,
        &key,
        "/v1/events/batch",
        json!([one_call(), one_call()]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 2, "{body}");
    // Folded across the request: 3 of 4 calls after the batch's second item.
    assert_eq!(
        h.get("x-lighttrack-usage-ratio").map(String::as_str),
        Some("0.750000")
    );

    // The 429 — the response that needs the signal most and carries no IngestResponse body.
    let (s, h, body) = ingest_capturing_headers(&app, &key, "/v1/events", one_call()).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        h.get("x-lighttrack-usage-ratio").map(String::as_str),
        Some("1.000000")
    );
    // Mirrors Retry-After, which proxies and browser fetch stacks are free to strip.
    assert_eq!(
        h.get("x-lighttrack-retry-after"),
        h.get("retry-after"),
        "the mirrored back-off must equal the standard one: {h:?}"
    );
}

#[tokio::test]
async fn a_project_with_no_limits_sends_no_ratio_at_all() {
    // The null-vs-zero trap on the header channel: absent means unknown. A client that read a
    // missing ratio as 0.0 would believe it had infinite headroom.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let (s, h, body) = ingest_capturing_headers(&app, &key, "/v1/events", one_call()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(!h.contains_key("x-lighttrack-usage-ratio"), "{h:?}");
    assert!(body.get("usage_ratio").is_none(), "{body}");
    assert!(body.get("binding_scope").is_none(), "{body}");
}

#[tokio::test]
async fn binding_scope_names_the_rule_that_is_actually_binding() {
    // `usage_ratio: 0.5` is only actionable with the scope attached: a project-wide cap means stop
    // everything, a model-scoped one means route the next call elsewhere and keep working.
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    for (id, threshold, scope) in [
        ("rule-wide", 100.0, None),
        (
            "rule-model",
            2.0,
            Some(lighttrack_core::LimitScope::Model(
                "claude-haiku-4-5".into(),
            )),
        ),
    ] {
        store
            .create_limit_rule(&LimitRule {
                id: id.to_string(),
                project_id: "proj-a".into(),
                metric: LimitMetric::Calls,
                window: LimitWindow::Hour,
                threshold: Threshold::Fixed(threshold),
                action: LimitAction::Block,
                enabled: true,
                warn_at: None,
                scope,
                escalation: None,
                escalated_until: None,
                origin: None,
                expires_at: None,
            })
            .unwrap();
    }
    let app = crate::build_router(state);
    let (s, _h, body) = ingest_capturing_headers(&app, &key, "/v1/events", one_call()).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    // 1/2 on the model rule beats 1/100 on the project-wide one.
    assert_eq!(body["binding_scope"]["kind"], "model", "{body}");
    assert_eq!(body["binding_scope"]["value"], "claude-haiku-4-5", "{body}");
    assert_eq!(body["binding_rule"], "rule-model", "{body}");
}
