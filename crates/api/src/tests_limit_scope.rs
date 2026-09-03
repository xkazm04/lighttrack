//! End-to-end tests for per-key budgets and the per-dimension usage surface.
//!
//! Driven through the wired axum router, so they pin the whole chain: auth resolves a key to its id,
//! ingest stamps that id (and strips a forged one), a rule scoped to `{"api_key": id}` binds only
//! that key's traffic, and `GET /v1/limits/usage` names the spender both before and after the cap
//! trips. The regression guard for the pre-existing dimensions lives here too.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{
    new_id, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, Threshold,
};
use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{add_key, ingest, make_key, setup};

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
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

/// A $0.50-ish event body (haiku @ $1/$5 per Mtok → 100k in + 80k out = $0.50).
fn body(extra_meta: Value) -> Value {
    json!({
        "provider": "anthropic", "model": "claude-haiku-4-5", "name": "summarize",
        "usage": { "input": 100_000, "output": 80_000 },
        "metadata": extra_meta
    })
}

fn key_rule(project: &str, key_id: &str, threshold: f64) -> LimitRule {
    LimitRule {
        id: new_id(),
        project_id: project.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Day,
        threshold: Threshold::Fixed(threshold),
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: Some(LimitScope::ApiKey(key_id.into())),
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
    }
}

/// Find one entry of a `/v1/limits/usage` response by its dimension value.
fn entry<'a>(resp: &'a Value, value: Option<&str>) -> &'a Value {
    resp["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["value"].as_str() == value)
        .unwrap_or_else(|| panic!("no entry for {value:?} in {resp}"))
}

#[tokio::test]
async fn a_staging_key_gets_its_own_budget_while_production_keeps_spending() {
    let (state, store) = setup(Redactor::off());
    let prod_token = make_key(&store, "proj-a"); // creates the project + its first key
    let (staging_id, staging_token) = add_key(&store, "proj-a", "staging");

    // $1.20/day on the staging key only — the thing that was impossible before: two keys of one
    // project with genuinely different budgets.
    store
        .create_limit_rule(&key_rule("proj-a", &staging_id, 1.2))
        .unwrap();

    let app = crate::build_router(state);

    // Two staging calls ($0.50 each) admit; the third reaches $1.50 >= $1.20 and is refused.
    for i in 0..2 {
        let (s, _) = ingest(&app, &staging_token, body(Value::Null)).await;
        assert_eq!(s, StatusCode::OK, "staging call {i} should admit");
    }
    let (s, v) = ingest(&app, &staging_token, body(Value::Null)).await;
    assert_eq!(
        s,
        StatusCode::TOO_MANY_REQUESTS,
        "staging hit its own cap: {v}"
    );

    // Production is untouched by staging's cap, at any volume.
    for i in 0..6 {
        let (s, _) = ingest(&app, &prod_token, body(Value::Null)).await;
        assert_eq!(
            s,
            StatusCode::OK,
            "prod call {i} must not be charged to staging's budget"
        );
    }
    // ...and staging is still capped afterwards (production's spend didn't relieve it either).
    let (s, _) = ingest(&app, &staging_token, body(Value::Null)).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn usage_by_key_answers_who_is_spending_before_any_rule_exists() {
    let (state, store) = setup(Redactor::off());
    let prod_token = make_key(&store, "proj-a");
    let (staging_id, staging_token) = add_key(&store, "proj-a", "staging");
    let app = crate::build_router(state);

    for _ in 0..3 {
        assert_eq!(
            ingest(&app, &staging_token, body(Value::Null)).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        ingest(&app, &prod_token, body(Value::Null)).await.0,
        StatusCode::OK
    );

    // No limit rule has ever been created for this project.
    assert!(store.list_limit_rules("proj-a", false).unwrap().is_empty());

    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=api_key",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["by"], "api_key");
    let staging = entry(&v, Some(&staging_id));
    assert_eq!(staging["calls"], 3);
    assert!((staging["cost_usd"].as_f64().unwrap() - 1.5).abs() < 1e-9);
    assert!((staging["cost_share_pct"].as_f64().unwrap() - 75.0).abs() < 1e-6);
    // The admin sees a human name; no rule binds the key yet, so no `rules` block.
    assert!(
        staging["label"].as_str().unwrap().starts_with("staging ("),
        "admin sees the key's name: {staging}"
    );
    assert!(
        staging.get("rules").is_none(),
        "no rule exists yet: {staging}"
    );
}

#[tokio::test]
async fn a_breach_names_its_key_through_the_api_not_only_an_alert_channel() {
    let (state, store) = setup(Redactor::off());
    let _prod = make_key(&store, "proj-a");
    let (staging_id, staging_token) = add_key(&store, "proj-a", "staging");
    // Observe-only, so the breaching event is actually recorded: an enforcing cap refuses the event
    // that would cross it, so *stored* usage never reaches the threshold — the same reason
    // `/v1/limits/status` reports an enforcing rule as "at 0.83 of cap" rather than breached.
    store
        .create_limit_rule(&LimitRule {
            action: LimitAction::Alert,
            ..key_rule("proj-a", &staging_id, 0.75)
        })
        .unwrap();
    let app = crate::build_router(state);

    for _ in 0..2 {
        assert_eq!(
            ingest(&app, &staging_token, body(Value::Null)).await.0,
            StatusCode::OK
        );
    }

    // No webhook is configured in this test process; the answer still has to be reachable.
    assert!(
        !state_alerts_enabled(),
        "this test must not depend on an alert channel"
    );
    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=api_key",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let e = entry(&v, Some(&staging_id));
    let rules = e["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules[0]["breached"], true,
        "the breached rule is reported against the key: {e}"
    );
    assert_eq!(rules[0]["scope"]["api_key"], staging_id.as_str());
}

/// Whether the process-wide alert config would deliver anything (it must not, for the test above).
fn state_alerts_enabled() -> bool {
    std::env::var("LIGHTTRACK_ALERT_WEBHOOK").is_ok()
        || std::env::var("LIGHTTRACK_ALERT_NTFY").is_ok()
}

#[tokio::test]
async fn a_client_cannot_forge_the_key_it_is_billed_as() {
    let (state, store) = setup(Redactor::off());
    let _prod = make_key(&store, "proj-a");
    let (staging_id, staging_token) = add_key(&store, "proj-a", "staging");
    let (victim_id, _victim_token) = add_key(&store, "proj-a", "victim");
    // Cap staging tightly; leave the victim key uncapped.
    store
        .create_limit_rule(&key_rule("proj-a", &staging_id, 0.6))
        .unwrap();
    let app = crate::build_router(state);

    // Staging claims to be the victim key. If the body were trusted, this would both dodge staging's
    // cap and charge the victim.
    let (s, _) = ingest(
        &app,
        &staging_token,
        body(json!({ "api_key_id": victim_id })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "first call is under the cap either way");
    let (s, _) = ingest(
        &app,
        &staging_token,
        body(json!({ "api_key_id": victim_id })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::TOO_MANY_REQUESTS,
        "the forged id must not dodge staging's own cap"
    );

    let (_, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=api_key",
    )
    .await;
    assert_eq!(
        entry(&v, Some(&staging_id))["calls"],
        1,
        "spend is attributed to the real key"
    );
    assert!(
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["value"].as_str() != Some(&victim_id)),
        "the victim key was never charged: {v}"
    );
}

#[tokio::test]
async fn an_admin_written_event_is_unattributed_rather_than_borrowing_a_key() {
    let (state, store) = setup(Redactor::off());
    let _prod = make_key(&store, "proj-a");
    let (staging_id, _staging_token) = add_key(&store, "proj-a", "staging");
    let app = crate::build_router(state);

    let mut b = body(json!({ "api_key_id": staging_id }));
    b["project_id"] = json!("proj-a");
    assert_eq!(ingest(&app, "admin-secret", b).await.0, StatusCode::OK);

    let (_, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=api_key",
    )
    .await;
    // The admin principal is not a key: its traffic lands in the unattributed bucket, and the id it
    // tried to claim is nowhere.
    assert_eq!(entry(&v, None)["calls"], 1);
    assert!(v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["value"].as_str() != Some(&staging_id)));
}

#[tokio::test]
async fn a_project_key_reading_the_breakdown_sees_ids_but_no_sibling_names() {
    let (state, store) = setup(Redactor::off());
    let prod_token = make_key(&store, "proj-a");
    let (staging_id, staging_token) = add_key(&store, "proj-a", "staging");
    let app = crate::build_router(state);
    assert_eq!(
        ingest(&app, &staging_token, body(Value::Null)).await.0,
        StatusCode::OK
    );

    let (s, v) = get(&app, &prod_token, "/v1/limits/usage?by=api_key").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let e = entry(&v, Some(&staging_id));
    assert!(
        e.get("label").is_none(),
        "a project key gets no roster of its siblings: {e}"
    );
    // ...and it certainly cannot read another project.
    let (s, _) = get(
        &app,
        &prod_token,
        "/v1/limits/usage?project=other&by=api_key",
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn customer_scoped_budgets_read_the_existing_billing_linkage() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            scope: Some(LimitScope::Customer("acme".into())),
            ..key_rule("proj-a", "unused", 0.6)
        })
        .unwrap();
    let app = crate::build_router(state);

    assert_eq!(
        ingest(&app, &token, body(json!({ "customer_id": "acme" })))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        ingest(&app, &token, body(json!({ "customer_id": "acme" })))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Another customer, and untagged traffic, are untouched.
    assert_eq!(
        ingest(&app, &token, body(json!({ "customer_id": "other" })))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        ingest(&app, &token, body(Value::Null)).await.0,
        StatusCode::OK
    );

    let (_, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=customer",
    )
    .await;
    assert_eq!(
        entry(&v, Some("acme"))["calls"],
        1,
        "only the admitted acme call is stored"
    );
    assert_eq!(
        entry(&v, None)["calls"],
        1,
        "untagged traffic keeps its own bucket"
    );
}

#[tokio::test]
async fn model_and_name_scoped_rules_behave_exactly_as_before() {
    // Regression guard for the pre-existing dimensions: adding api_key/customer must not shift them.
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            scope: Some(LimitScope::Model("claude-haiku-4-5".into())),
            metric: LimitMetric::Calls,
            threshold: Threshold::Fixed(2.0),
            ..key_rule("proj-a", "unused", 1.0)
        })
        .unwrap();
    store
        .create_limit_rule(&LimitRule {
            scope: Some(LimitScope::Name("other-usecase".into())),
            metric: LimitMetric::Calls,
            threshold: Threshold::Fixed(1.0),
            ..key_rule("proj-a", "unused", 1.0)
        })
        .unwrap();
    let app = crate::build_router(state);

    // The model cap binds (2 calls); the name cap never applies (our events are `summarize`).
    assert_eq!(
        ingest(&app, &token, body(Value::Null)).await.0,
        StatusCode::OK
    );
    let (s, v) = ingest(&app, &token, body(Value::Null)).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    // A different model is unaffected.
    let mut other = body(Value::Null);
    other["model"] = json!("gpt-4o");
    other["provider"] = json!("openai");
    assert_eq!(ingest(&app, &token, other).await.0, StatusCode::OK);

    // The status surface still reports both scoped rules unchanged.
    let (s, v) = get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await;
    assert_eq!(s, StatusCode::OK);
    let scopes: Vec<&Value> = v["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| &s["scope"])
        .collect();
    assert!(
        scopes.contains(&&json!({ "model": "claude-haiku-4-5" })),
        "{v}"
    );
    assert!(scopes.contains(&&json!({ "name": "other-usecase" })), "{v}");
}

#[tokio::test]
async fn an_unknown_dimension_is_a_400_not_a_silent_default() {
    let (state, store) = setup(Redactor::off());
    let _t = make_key(&store, "proj-a");
    let app = crate::build_router(state);
    let (s, _) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=nonsense",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&window=fortnight",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
