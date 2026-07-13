//! End-to-end test for the predictive forecast surface, over the wired axum router.
//!
//! Ingest ten days of *rising* daily cost for one customer (`acme`), attach revenue that currently
//! covers it, configure a daily cost budget the trend will cross in the future, then read
//! `GET /v1/forecast` back and confirm it (a) projects the spend, (b) forecasts the budget breach
//! with a positive ETA, and (c) flags the customer as on track to turn unprofitable — the two
//! headline pre-emptive alerts.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use lighttrack_core::{
    new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, RevenueEvent, RevenueKind,
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

#[tokio::test]
async fn forecast_projects_budget_breach_and_margin_erosion() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");

    // A daily cost budget of $15 — Alert-only so ingest is never blocked while we backfill history.
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: "proj-a".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold: 15.0,
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
        })
        .unwrap();

    // Revenue that currently covers acme ($120 one-time, recognized in-window) → ~$12/day.
    store
        .insert_revenue_event(&RevenueEvent {
            id: new_id(),
            project_id: "proj-a".into(),
            source: "manual".into(),
            external_id: None,
            customer_id: Some("acme".into()),
            product_id: None,
            amount_usd: 120.0,
            currency: "USD".into(),
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts: Utc::now() - Duration::days(5),
        })
        .unwrap();

    let app = crate::build_router(state);

    // Ten days of rising daily cost for acme: $1/day nine days ago … $10/day today.
    let now = Utc::now();
    for i in 0..10u32 {
        let ts = (now - Duration::days((9 - i) as i64)).to_rfc3339();
        let cost = (i + 1) as f64;
        let (status, v) = ingest(
            &app,
            &key,
            json!({
                "provider": "anthropic",
                "model": "claude-haiku-4-5",
                "usage": { "input": 10, "output": 5 },
                "cost_usd": cost,
                "ts": ts,
                "metadata": { "customer_id": "acme" }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "ingest day {i} failed: {v}");
    }

    let (status, f) = get(
        &app,
        &key,
        "/v1/forecast?project=proj-a&by=customer&lookback=10&horizon=14",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{f}");

    // (a) Spend is projected forward and is positive given the rising trend.
    assert!(
        f["spend"]["projected_daily_cost_usd"].as_f64().unwrap() > 0.0,
        "{f}"
    );
    assert!(
        f["spend"]["cost_trend"]["slope"].as_f64().unwrap() > 0.0,
        "trend should be rising: {f}"
    );

    // (b) The daily cost budget is forecast to breach at some point in the future (eta > 0).
    let budgets = f["budgets"].as_array().unwrap();
    let budget = budgets
        .iter()
        .find(|b| b["metric"] == "cost_usd")
        .expect("a cost budget forecast");
    let eta = budget["eta_days"].as_f64().expect("a future breach ETA");
    assert!(
        eta > 0.0 && eta <= 14.0,
        "budget eta out of range: {budget}"
    );

    // (c) acme is currently profitable but on track to turn unprofitable.
    let margins = f["margins"].as_array().unwrap();
    let acme = margins
        .iter()
        .find(|m| m["key"] == "acme")
        .expect("a margin forecast for acme");
    assert_eq!(acme["currently_profitable"], true, "{acme}");
    assert!(
        acme["eta_unprofitable_days"].as_f64().is_some(),
        "expected a crossover ETA: {acme}"
    );

    // The two headline pre-emptive alerts are present.
    let alerts = f["alerts"].as_array().unwrap();
    assert!(
        alerts.iter().any(|a| a["kind"] == "budget_breach"),
        "missing budget_breach alert: {f}"
    );
    assert!(
        alerts
            .iter()
            .any(|a| a["kind"] == "margin_erosion" && a["subject"] == "acme"),
        "missing margin_erosion alert for acme: {f}"
    );
}

#[tokio::test]
async fn forecast_is_quiet_with_no_history() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let app = crate::build_router(state);

    let (status, f) = get(&app, &key, "/v1/forecast?project=proj-a").await;
    assert_eq!(status, StatusCode::OK, "{f}");
    // No traffic, no limits, no revenue → no forecasts and no alerts (a flat zero series).
    assert!(f["budgets"].as_array().unwrap().is_empty(), "{f}");
    assert!(f["margins"].as_array().unwrap().is_empty(), "{f}");
    assert!(f["alerts"].as_array().unwrap().is_empty(), "{f}");
    assert_eq!(
        f["spend"]["projected_daily_cost_usd"].as_f64().unwrap(),
        0.0,
        "{f}"
    );
}

#[tokio::test]
async fn forecast_requires_a_project() {
    let (state, _store) = setup(Redactor::off());
    let app = crate::build_router(state);
    // Admin key with no project query param → 400 (forecasting is per-project).
    let (status, _) = get(&app, "admin-secret", "/v1/forecast").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
