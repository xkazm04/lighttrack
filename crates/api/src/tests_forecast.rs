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
    new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, RevenueEvent, RevenueKind, Threshold,
    ThresholdDimension,
};
use lighttrack_store::Store;

use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

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
            threshold: Threshold::Fixed(15.0),
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
            amount_minor: None,
            fx_rate: None,
            fx_book_version: None,
            converted: None,
            kind: RevenueKind::OneTime,
            period_start: None,
            period_end: None,
            ts: Utc::now() - Duration::days(5),
        })
        .unwrap();

    let app = crate::build_router(state);

    // Ten days of rising daily cost for acme: $1/day nine days ago … $10/day today.
    //
    // Seeded straight into the store rather than POSTed with a backdated `ts`, because the daily
    // series buckets on **server arrival time** — a forecast a caller could reshape by backdating its
    // own events would not be worth alerting on. So "ten real days of traffic" is exactly what this
    // writes: each event's arrival stamped on its own day.
    let now = Utc::now();
    for i in 0..10u32 {
        let day = now - Duration::days((9 - i) as i64);
        let mut e: lighttrack_core::LlmEvent = serde_json::from_value(json!({
            "id": new_id(),
            "project_id": "proj-a",
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 },
            "cost_usd": (i + 1) as f64,
            "ts": day.to_rfc3339(),
            "metadata": { "customer_id": "acme" }
        }))
        .unwrap();
        e.received_at = day;
        store.insert_event(&e).unwrap();
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

    // Ten observed days spanning ten: nothing is withheld, and the fit publishes its confidence.
    assert!(
        f["refused"].as_array().unwrap().is_empty(),
        "an established series refuses nothing: {f}"
    );
    let conf = f["spend"]["confidence"]
        .as_f64()
        .expect("a presentable fit publishes r²");
    assert!(conf > 0.9, "a clean linear ramp fits well: {conf}");

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
    // The message carries the confidence it was allowed to publish.
    let budget_alert = alerts
        .iter()
        .find(|a| a["kind"] == "budget_breach")
        .unwrap();
    assert!(
        budget_alert["message"].as_str().unwrap().contains("r²="),
        "a gated alert states its confidence: {budget_alert}"
    );
}

/// The defect this gate exists for: two days of steeply rising spend inside a fourteen-day window
/// used to be fitted over twelve zero-filled days and paged as "on track to breach". It must now
/// come back as a **named refusal** — not as an alert, and not as silence either, which an operator
/// would read as "all is well".
#[tokio::test]
async fn a_project_with_two_days_of_history_refuses_instead_of_forecasting() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");

    let rule_id = new_id();
    store
        .create_limit_rule(&LimitRule {
            id: rule_id.clone(),
            project_id: "proj-a".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold: Threshold::Fixed(50.0),
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

    // $2 yesterday, $20 today — a 10× ramp, and exactly the kind of two-point line that fits itself
    // perfectly and means nothing.
    let now = Utc::now();
    for (days_ago, cost) in [(1i64, 2.0f64), (0, 20.0)] {
        let day = now - Duration::days(days_ago);
        let mut e: lighttrack_core::LlmEvent = serde_json::from_value(json!({
            "id": new_id(),
            "project_id": "proj-a",
            "provider": "anthropic",
            "model": "claude-haiku-4-5",
            "usage": { "input": 10, "output": 5 },
            "cost_usd": cost,
            "ts": day.to_rfc3339(),
            "metadata": { "customer_id": "acme" }
        }))
        .unwrap();
        e.received_at = day;
        store.insert_event(&e).unwrap();
    }

    let (status, f) = get(
        &app,
        &key,
        "/v1/forecast?project=proj-a&lookback=14&horizon=14",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{f}");

    let budget = f["budgets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["rule_id"] == rule_id.as_str())
        .expect("the rule is still forecast, just not presented");
    assert!(
        budget["eta_days"].is_null(),
        "an ETA under the evidence floor is withheld, not published: {budget}"
    );
    assert!(
        budget["trend"]["r2"].is_null() && f["spend"]["confidence"].is_null(),
        "no confidence is attached to a refused fit: {f}"
    );
    assert!(
        f["alerts"].as_array().unwrap().is_empty(),
        "nobody is paged on two days of history: {f}"
    );

    let refused = f["refused"].as_array().unwrap();
    assert!(
        refused.iter().any(|r| r["subject"] == rule_id.as_str()),
        "the withheld budget names itself in refused[]: {f}"
    );
    let spend = refused
        .iter()
        .find(|r| r["subject"] == "spend")
        .expect("the spend projection is refused too");
    assert_eq!(
        spend["reason"], "4 observed days needed, 2 seen",
        "the reason is written for an operator to read: {spend}"
    );

    // And the clamp: asking for a two-day lookback cannot buy a forecast the floor would refuse.
    let (status, f) = get(&app, &key, "/v1/forecast?project=proj-a&lookback=2").await;
    assert_eq!(status, StatusCode::OK, "{f}");
    assert_eq!(
        f["lookback_days"], 4,
        "lookback is clamped to the floor: {f}"
    );
}

/// A revenue-share cap has no fixed figure, so `nominal_threshold()` is infinity: the budget row used
/// to come back with `threshold: null`, no ETA and no entry in `refused[]` — silence that reads as
/// "no risk". It is now a named refusal and no budget row at all.
#[tokio::test]
async fn a_revenue_share_budget_is_refused_by_name_not_forecast_against_infinity() {
    let (state, store) = setup(Redactor::off());
    let key = make_key(&store, "proj-a");
    let rule_id = new_id();
    store
        .create_limit_rule(&LimitRule {
            id: rule_id.clone(),
            project_id: "proj-a".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Month,
            threshold: Threshold::RevenueShare {
                pct: 80.0,
                dimension: ThresholdDimension::Customer,
            },
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

    let (status, f) = get(&app, &key, "/v1/forecast?project=proj-a").await;
    assert_eq!(status, StatusCode::OK, "{f}");
    assert!(
        f["budgets"].as_array().unwrap().is_empty(),
        "no row with a null threshold: {f}"
    );
    let refused = f["refused"].as_array().unwrap();
    let r = refused
        .iter()
        .find(|r| r["subject"] == rule_id.as_str())
        .expect("the revenue-share rule names itself in refused[]");
    assert!(
        r["reason"].as_str().unwrap().contains("revenue-share"),
        "{r}"
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
