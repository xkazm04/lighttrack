//! A model cap must count the traffic it would reject.
//!
//! `LimitScope::matches` decides a `Model` rule *applies* by canonical family — a `gpt-4o` cap
//! catches `gpt-4o-2024-08-06`. The ledger behind `/v1/limits/status` and `/v1/limits/usage` used
//! to decide the number that rule is *evaluated against* with raw string equality, so a cap over
//! dated-snapshot traffic reported `current ≈ 0`, never warned, never breached, and still read as
//! enforcing. These tests drive the wired router so they pin the gate and the ledger together:
//! **one rule, evaluated twice, must agree.**

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
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

/// An OpenAI event with its cost stated by the caller, so these tests are about membership rather
/// than about what the price book happens to hold for a dated snapshot.
fn ev(model: &str, cost: f64) -> Value {
    json!({
        "provider": "openai", "model": model, "name": "summarize",
        "usage": { "input": 10, "output": 5 },
        "cost_usd": cost
    })
}

fn model_cap(project: &str, model: &str, threshold: f64, action: LimitAction) -> LimitRule {
    LimitRule {
        id: new_id(),
        project_id: project.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Day,
        threshold: Threshold::Fixed(threshold),
        action,
        enabled: true,
        warn_at: None,
        scope: Some(LimitScope::Model(model.into())),
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
    }
}

/// The status entry for a rule scoped to `{"model": <model>}`.
fn status_of<'a>(resp: &'a Value, model: &str) -> &'a Value {
    resp["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scope"] == json!({ "model": model }))
        .unwrap_or_else(|| panic!("no status for model={model} in {resp}"))
}

/// CASE 1 — the headline: $7 of `gpt-4o-2024-08-06` under a $5/day `gpt-4o` cap must read as
/// breached on the status surface. Observe-only so the breaching spend is actually stored (an
/// enforcing cap refuses the event that would cross it, so stored usage never reaches the
/// threshold — the same reason the existing per-key breach test uses `Alert`).
#[tokio::test]
async fn a_model_cap_reports_the_dated_snapshot_spend_as_breached() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&model_cap("proj-a", "gpt-4o", 5.0, LimitAction::Alert))
        .unwrap();
    let app = crate::build_router(state);

    for _ in 0..2 {
        assert_eq!(
            ingest(&app, &token, ev("gpt-4o-2024-08-06", 3.5)).await.0,
            StatusCode::OK
        );
    }

    let (s, v) = get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let st = status_of(&v, "gpt-4o");
    let current = st["current"].as_f64().unwrap();
    assert!(
        current >= 7.0 - 1e-9,
        "a `gpt-4o` cap must count the dated snapshot it would reject: current = {current}, \
         expected >= 7.0 (raw-equality ledger reports ~0.0): {st}"
    );
    assert_eq!(
        st["breached"], true,
        "$7 of in-family spend against a $5 cap is a breach: {st}"
    );
}

/// CASE 2 — the gate and the ledger agree about membership.
///
/// The **429 half is green before** on SQLite by design: admission folds the canonical family in
/// `sqlite/usage_cache.rs`, so the rejection already happens and already names the scope. The
/// **report half is the red-before assertion**: after the rejection, the status surface must show
/// the same stored in-family total the gate counted, not 0.
#[tokio::test]
async fn the_rejecting_gate_and_the_status_surface_count_the_same_family() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&model_cap("proj-a", "gpt-4o", 5.0, LimitAction::Block))
        .unwrap();
    let app = crate::build_router(state);

    // $3.5 stored, under the cap.
    assert_eq!(
        ingest(&app, &token, ev("gpt-4o-2024-08-06", 3.5)).await.0,
        StatusCode::OK
    );
    // The next one would take the family total to $7 >= $5, so it is rejected.
    let (s, body) = ingest(&app, &token, ev("gpt-4o-2024-08-06", 3.5)).await;
    assert_eq!(
        s,
        StatusCode::TOO_MANY_REQUESTS,
        "admission folds the family (green before this change): {body}"
    );
    let reason = serde_json::to_string(&body).unwrap();
    assert!(
        reason.contains("[scope model=gpt-4o]"),
        "the breach reason names the scope that rejected: {body}"
    );
    // The rejection names the number the gate counted: $3.5 stored + the $3.5 candidate.
    assert!(
        reason.contains("7.0000 >= 5.0000"),
        "the gate counted the in-family total, not the raw-equality one: {body}"
    );
    let rejected_current = 7.0_f64;

    // The report half: the status surface sees the same membership, i.e. the stored in-family
    // total the gate counted ($3.5) — the candidate was refused, so it is not in the ledger.
    let (s, v) = get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let st = status_of(&v, "gpt-4o");
    let reported = st["current"].as_f64().unwrap();
    assert!(
        (reported - 3.5).abs() < 1e-9,
        "the status surface must report the stored spend the gate counted: {reported}, \
         expected 3.5 (raw-equality ledger reports 0.0): {st}"
    );
    assert!(
        (rejected_current - reported - 3.5).abs() < 1e-9,
        "the two surfaces differ by exactly the refused candidate, not by a membership rule"
    );
}

/// GUARD — a different canonical family is still outside the scope.
#[tokio::test]
async fn a_different_family_is_admitted_and_uncounted_under_the_cap() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&model_cap("proj-a", "gpt-4o", 5.0, LimitAction::Alert))
        .unwrap();
    let app = crate::build_router(state);

    assert_eq!(
        ingest(&app, &token, ev("gpt-4o-mini", 9.0)).await.0,
        StatusCode::OK,
        "gpt-4o-mini is its own family: a gpt-4o cap does not bind it"
    );

    let (s, v) = get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let st = status_of(&v, "gpt-4o");
    assert!(
        st["current"].as_f64().unwrap().abs() < 1e-9,
        "gpt-4o-mini spend must not appear in the gpt-4o cap's current: {st}"
    );
    assert_eq!(st["breached"], false, "{st}");
}

/// GUARD — exact-value dimensions keep exact semantics. A prefix is not membership for a
/// use-case name or a billing customer; only `Model` folds, and only to the canonical family.
#[tokio::test]
async fn exact_value_dimensions_are_not_widened_to_a_prefix() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            scope: Some(LimitScope::Customer("cus_123".into())),
            ..model_cap("proj-a", "unused", 1.0, LimitAction::Alert)
        })
        .unwrap();
    store
        .create_limit_rule(&LimitRule {
            scope: Some(LimitScope::Name("summarize".into())),
            ..model_cap("proj-a", "unused", 1.0, LimitAction::Alert)
        })
        .unwrap();
    let app = crate::build_router(state);

    let mut e = ev("gpt-4o", 9.0);
    e["name"] = json!("summarize-v2");
    e["metadata"] = json!({ "customer_id": "cus_123x" });
    assert_eq!(ingest(&app, &token, e).await.0, StatusCode::OK);

    let (s, v) = get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for (kind, value) in [("customer", "cus_123"), ("name", "summarize")] {
        let st = v["statuses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["scope"] == json!({ kind: value }))
            .unwrap_or_else(|| panic!("no status for {kind}={value} in {v}"));
        assert!(
            st["current"].as_f64().unwrap().abs() < 1e-9,
            "`{value}` must not count `{value}x`-style near-misses: {st}"
        );
    }
}

/// CASE 5 — `/v1/limits/usage?by=model` is the one surface built to answer "which caps bind this
/// spend". It must attach the `gpt-4o` rule to the `gpt-4o-2024-08-06` row.
#[tokio::test]
async fn the_model_breakdown_attaches_the_cap_that_binds_the_dated_row() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&model_cap("proj-a", "gpt-4o", 5.0, LimitAction::Alert))
        .unwrap();
    let app = crate::build_router(state);

    assert_eq!(
        ingest(&app, &token, ev("gpt-4o-2024-08-06", 7.0)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        ingest(&app, &token, ev("gpt-4o-mini", 1.0)).await.0,
        StatusCode::OK
    );

    let (s, v) = get(
        &app,
        "admin-secret",
        "/v1/limits/usage?project=proj-a&by=model",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = v["entries"].as_array().unwrap();
    let dated = rows
        .iter()
        .find(|e| e["value"] == json!("gpt-4o-2024-08-06"))
        .unwrap_or_else(|| panic!("the dated row is in the breakdown: {v}"));
    let rules = dated["rules"]
        .as_array()
        .unwrap_or_else(|| panic!("the dated row names the cap that binds it: {dated}"));
    assert_eq!(
        rules.len(),
        1,
        "the gpt-4o cap binds the gpt-4o-2024-08-06 row (today this array is empty, so the one \
         surface built to say which caps bind the spend says none do): {dated}"
    );
    assert_eq!(rules[0]["scope"], json!({ "model": "gpt-4o" }));

    // And the other family's row is not claimed by it.
    let mini = rows
        .iter()
        .find(|e| e["value"] == json!("gpt-4o-mini"))
        .unwrap_or_else(|| panic!("the mini row is in the breakdown: {v}"));
    assert!(
        mini.get("rules").is_none() || mini["rules"].as_array().unwrap().is_empty(),
        "no cap binds gpt-4o-mini: {mini}"
    );
}

/// GUARD — an unscoped rule's `current` is untouched: the project-wide total still counts every
/// family, and the fold is reached only through a `Model` scope.
#[tokio::test]
async fn an_unscoped_rule_still_counts_every_family() {
    let (state, store) = setup(Redactor::off());
    let token = make_key(&store, "proj-a");
    store
        .create_limit_rule(&LimitRule {
            scope: None,
            ..model_cap("proj-a", "unused", 100.0, LimitAction::Alert)
        })
        .unwrap();
    let app = crate::build_router(state);

    for (model, cost) in [("gpt-4o", 1.0), ("gpt-4o-2024-08-06", 2.0), ("o3", 4.0)] {
        assert_eq!(
            ingest(&app, &token, ev(model, cost)).await.0,
            StatusCode::OK
        );
    }

    let (s, v) = get(&app, "admin-secret", "/v1/limits/status?project=proj-a").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let st = v["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["scope"].is_null())
        .unwrap_or_else(|| panic!("the unscoped rule is reported: {v}"));
    assert!(
        (st["current"].as_f64().unwrap() - 7.0).abs() < 1e-9,
        "the project-wide total is unchanged at 1+2+4 = 7.0: {st}"
    );

    // Byte-identical to the store's own project-wide read for the same window.
    let direct = store
        .usage_since("proj-a", LimitWindow::Day.since(chrono::Utc::now()))
        .unwrap();
    assert!((direct.cost_usd - 7.0).abs() < 1e-9);
    assert_eq!(direct.calls, 3);
}
