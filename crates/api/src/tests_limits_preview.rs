//! `GET /v1/limits/preview` end to end: an operator learns what a threshold would have done to
//! their own traffic, and learns it without creating anything.
//!
//! These drive the wired router, so they also pin the two properties the route is only useful if it
//! has: it is genuinely side-effect-free, and it is declared in the contract table (an undeclared
//! route answers HTTP 500 under the access layer, which would make the whole feature a 500).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use lighttrack_store::Store;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use crate::redact::Redactor;
use crate::tests_ingest::{ingest, make_key, setup};

async fn get(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
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

/// An event with its cost stated by the caller, so these tests are about the replay rather than
/// about what the price book holds.
fn ev(provider: &str, model: &str, cost: f64) -> Value {
    json!({
        "provider": provider, "model": model, "name": "summarize",
        "usage": { "input": 10, "output": 5 },
        "cost_usd": cost
    })
}

/// Seed a project whose rolling daily cost is $7.20, all of it inside the current window.
async fn seeded(app: &Router, token: &str) {
    for c in [3.0, 2.2, 2.0] {
        assert_eq!(
            ingest(app, token, ev("openai", "gpt-4o", c)).await.0,
            StatusCode::OK
        );
    }
}

/// CASE 1 — the headline. A $5/day cap against a window that peaked at $7.20 would have fired, and
/// the operator learns when, how hard, and for how long, without creating the rule.
#[tokio::test]
async fn a_tight_candidate_reports_the_breach_episodes_it_would_have_caused() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    seeded(&app, &token).await;

    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/preview?project=proj-a&metric=cost_usd&window=day&threshold=5&action=block&over=7d",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["would_breach"], true, "{v}");
    let peak = v["peak_ratio"].as_f64().unwrap();
    assert!(
        peak >= 1.44 - 1e-9,
        "peak ratio must be 7.20/5 = 1.44 or better, got {peak}: {v}"
    );

    let eps = v["episodes"].as_array().expect("episodes array");
    assert!(
        !eps.is_empty(),
        "a breach must be reported as an episode: {v}"
    );
    let from = v["over"]["from"].as_str().unwrap().to_string();
    let to = v["over"]["to"].as_str().unwrap().to_string();
    for e in eps {
        let (f, l) = (
            e["first_at"].as_str().unwrap(),
            e["last_at"].as_str().unwrap(),
        );
        assert!(
            f >= from.as_str() && l <= to.as_str(),
            "an episode must sit inside the replayed span [{from}, {to}]: {e}"
        );
        assert!(f <= l, "{e}");
    }
    // The timeline is the evidence behind the headline, not a separate claim.
    let tl = v["timeline"].as_array().expect("timeline");
    assert_eq!(tl.len(), 48, "the default grid: {}", tl.len());
    assert!(tl.iter().any(|s| s["breached"] == json!(true)));
    assert_eq!(v["simulated"], true, "a read-only what-if says so: {v}");
}

/// CASE 2 — the slack-cap answer. Learning a threshold is too high to ever bind is the other half
/// of the question, and it must come back as `false` with no episodes rather than as silence.
#[tokio::test]
async fn a_slack_candidate_reports_no_breach_without_creating_a_rule() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    seeded(&app, &token).await;

    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/preview?project=proj-a&metric=cost_usd&window=day&threshold=100&action=block&over=7d",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["would_breach"], false, "{v}");
    assert_eq!(v["episodes"], json!([]), "{v}");
    let peak = v["peak_ratio"].as_f64().unwrap();
    assert!(peak < 1.0, "a $100 cap over $7.20 of spend: {peak}");
    assert!(peak > 0.0, "and it is a measurement, not a zero: {v}");
}

/// CASE 3 (the HTTP half) — a `throttle` candidate publishes a shed ramp, and every step of it is a
/// fraction. The arithmetic itself is pinned in `lighttrack_core::limits::backtest`.
#[tokio::test]
async fn a_throttle_candidate_publishes_a_shed_ramp_on_every_step() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    seeded(&app, &token).await;

    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/preview?project=proj-a&metric=cost_usd&window=day&threshold=5&action=throttle&warn_at=0.5&over=7d",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["action"], json!("throttle"));
    assert_eq!(v["warn_at"], json!(0.5));
    let tl = v["timeline"].as_array().expect("timeline");
    for step in tl {
        let f = step["shed_fraction"]
            .as_f64()
            .unwrap_or_else(|| panic!("every step carries shed_fraction: {step}"));
        assert!((0.0..=1.0).contains(&f), "{step}");
    }
    assert!(
        tl.iter()
            .any(|s| s["shed_fraction"].as_f64().unwrap_or(0.0) > 0.0),
        "traffic past warn_at must show back-pressure: {v}"
    );
}

/// CASE 4 — a nonsensical threshold is the same 400 a create would give, carrying `validate`'s own
/// message. Never a 200 whose every ratio is infinity.
#[tokio::test]
async fn a_zero_threshold_is_the_same_400_a_create_would_give() {
    let (state, store) = setup(Redactor::off());
    let _ = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/preview?project=proj-a&threshold=0",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let msg = serde_json::to_string(&v).unwrap();
    assert!(
        msg.contains("threshold must be a finite number greater than 0 (got 0)"),
        "the preview reuses LimitRule::validate's own message: {v}"
    );
}

/// CASE 5 — a scoped candidate replays only its own traffic. The other provider's spend is absent
/// from every step, which is the whole point of previewing a scoped cap.
#[tokio::test]
async fn a_scoped_candidate_replays_only_the_traffic_its_scope_binds() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    assert_eq!(
        ingest(&app, &token, ev("openai", "gpt-4o", 4.0)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        ingest(&app, &token, ev("anthropic", "claude-haiku-4-5", 20.0))
            .await
            .0,
        StatusCode::OK
    );

    let scoped = get(
        &app,
        "admin-secret",
        "/v1/limits/preview?project=proj-a&threshold=5&action=block&over=7d&scope=%7B%22provider%22%3A%22openai%22%7D",
    )
    .await;
    assert_eq!(scoped.0, StatusCode::OK, "{}", scoped.1);
    let v = scoped.1;
    assert_eq!(v["scope"], json!({ "provider": "openai" }), "{v}");
    let peak = v["peak_ratio"].as_f64().unwrap();
    assert!(
        (peak - 4.0 / 5.0).abs() < 1e-9,
        "the openai-only rolling total is $4.00, not $24.00: peak = {peak}: {v}"
    );
    assert_eq!(v["would_breach"], false, "$4 does not breach a $5 cap: {v}");
    for step in v["timeline"].as_array().unwrap() {
        let c = step["current"].as_f64().unwrap();
        assert!(
            c < 20.0,
            "the anthropic spend must be absent from every step: {step}"
        );
    }

    // …and unscoped, the same candidate sees the whole project and breaches.
    let (s, all) = get(
        &app,
        "admin-secret",
        "/v1/limits/preview?project=proj-a&threshold=5&action=block&over=7d",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{all}");
    assert_eq!(
        all["would_breach"], true,
        "$24 across both providers breaches the same $5 cap: {all}"
    );
}

/// GUARD (CASE 6) — the preview persists nothing. A what-if that left a rule, an alert or a
/// rejection behind would be a write wearing a GET's clothes, and the MCP tool's `readOnlyHint`
/// would be a lie.
#[tokio::test]
async fn the_preview_persists_nothing_at_all() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    seeded(&app, &token).await;

    let before = (
        get(&app, "admin-secret", "/v1/projects/proj-a/limits").await,
        get(&app, "admin-secret", "/v1/alerts?project=proj-a").await,
        get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await,
    );

    for q in [
        "/v1/limits/preview?project=proj-a&threshold=1&action=block&over=7d",
        "/v1/limits/preview?project=proj-a&threshold=0.0001&action=throttle&warn_at=0.1&over=48h",
        "/v1/limits/preview?project=proj-a&metric=calls&threshold=1&action=block",
    ] {
        let (s, v) = get(&app, "admin-secret", q).await;
        assert_eq!(s, StatusCode::OK, "{q}: {v}");
        assert_eq!(v["would_breach"], true, "{q} must breach to be a real test");
    }

    let after = (
        get(&app, "admin-secret", "/v1/projects/proj-a/limits").await,
        get(&app, "admin-secret", "/v1/alerts?project=proj-a").await,
        get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await,
    );
    assert_eq!(before.0, after.0, "no rule was created");
    assert_eq!(before.1, after.1, "no alert was raised");
    assert_eq!(
        before.2 .1["rejected"], after.2 .1["rejected"],
        "no rejection was counted against the project"
    );
    assert_eq!(
        before.2 .1["statuses"], after.2 .1["statuses"],
        "the configured caps are untouched"
    );
    // The store itself: a replay must not have written an event either.
    assert_eq!(
        store
            .usage_since(
                "proj-a",
                lighttrack_core::LimitWindow::Day.since(chrono::Utc::now())
            )
            .unwrap()
            .calls,
        3,
        "the three seeded events and nothing more"
    );
}

/// GUARD (CASE 8) — the route is declared in `lighttrack_contract::endpoints()` with the access it
/// is served under, and it resolves through the access layer with that credential.
///
/// An undeclared route answers HTTP 500 under `access_layer`, so this is not a formality: without
/// the contract row the entire feature is a 500 in production and green in a router-less unit test.
#[tokio::test]
async fn the_route_is_declared_in_the_contract_and_resolves_through_the_access_layer() {
    let e = lighttrack_contract::endpoints()
        .find(|e| e.path == "/v1/limits/preview")
        .expect("GET /v1/limits/preview must be a row in the contract endpoint table");
    assert_eq!(e.method, lighttrack_contract::Method::Get);
    assert!(!e.mutating, "a replay is not a write");
    assert_eq!(
        e.access,
        lighttrack_contract::Access::Key(lighttrack_contract::KeyScope::Read),
        "the same credential /v1/limits/usage is served under"
    );
    let mcp = e.mcp.as_ref().expect("reachable from MCP");
    assert!(
        mcp.read_only,
        "a GET replay is a read tool, never a gated write"
    );

    // …and the declared credential actually reaches it, rather than the 500 an undeclared route
    // gives: a project read key, which is what `access: Key(Read)` promises.
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let (s, v) = get(&app, &token, "/v1/limits/preview?threshold=5").await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a read key must resolve the route, not meet an undeclared-route 500: {v}"
    );
    assert_eq!(
        v["project_id"],
        json!("proj-a"),
        "derived from the key: {v}"
    );
}
