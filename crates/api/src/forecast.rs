//! Predictive cost/margin forecasting surface.
//!
//! Turns the rolling daily counters the system already keeps into a forward look and pre-emptive
//! alerts: *which budgets are on track to breach, and when?* and *which customers/products are
//! trending unprofitable?* The math is pure ([`lighttrack_core::forecast`]); this module is wiring —
//! pull the daily series + limits + revenue, project, shape JSON, and fire best-effort alerts.

use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::forecast::{forecast_budget, forecast_margin, BudgetForecast, MarginForecast};
use lighttrack_core::margin::UNATTRIBUTED;
use lighttrack_core::{
    compute_margin, CostByDimension, LimitMetric, LimitRule, LimitWindow, MarginDimension,
    RevenueEvent, Trend,
};
use lighttrack_store::{DailyDimCost, DailyUsage, StoreError, Usage};

use crate::error::ApiError;
use crate::forecast_alerts::{build_alerts, ForecastAlert};
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// At most this many customers/products are forecast (the worst-margin ones first), to bound the
/// response and the per-key trend work.
const MAX_DIM_FORECASTS: usize = 50;

#[derive(Deserialize)]
pub(crate) struct ForecastParams {
    project: Option<String>,
    /// `customer` (default) | `product` — the billing axis for margin forecasting.
    by: Option<String>,
    /// How far ahead to project, in days (default 14, clamped to 1..=90).
    horizon: Option<u32>,
    /// How many trailing days of history to fit the trend over (default 14, clamped to 2..=90).
    lookback: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct SpendProjection {
    cost_trend: Trend,
    projected_daily_cost_usd: f64,
    projected_cost_next_7d_usd: f64,
    projected_cost_next_30d_usd: f64,
}

#[derive(Serialize)]
pub(crate) struct ForecastResponse {
    project_id: String,
    generated_at: DateTime<Utc>,
    dimension: String,
    horizon_days: u32,
    lookback_days: u32,
    spend: SpendProjection,
    budgets: Vec<BudgetForecast>,
    margins: Vec<MarginForecast>,
    /// Pre-emptive warnings derived from the forecasts (also delivered best-effort to alert sinks,
    /// by the handler and by the scheduled sweep alike).
    pub(crate) alerts: Vec<ForecastAlert>,
}

/// Raw store reads gathered in one blocking hop, before any pure shaping.
struct RawForecast {
    daily: Vec<DailyUsage>,
    rules: Vec<LimitRule>,
    window_usage: HashMap<LimitWindow, Usage>,
    revenue: Vec<RevenueEvent>,
    costs: Vec<CostByDimension>,
    daily_dim: Vec<DailyDimCost>,
}

pub(crate) async fn get_forecast(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ForecastParams>,
) -> Result<Json<ForecastResponse>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?
        .ok_or_else(|| ApiError::bad_request("project is required"))?;
    let dim = MarginDimension::parse(q.by.as_deref().unwrap_or("customer"));
    let horizon = q.horizon.unwrap_or(14).clamp(1, 90);
    let lookback = q.lookback.unwrap_or(14).clamp(2, 90);

    let resp = compute_forecast(&st, &project, dim, horizon, lookback).await?;
    if !resp.alerts.is_empty() {
        st.alerts.notify_forecast(&resp.alerts);
    }
    Ok(Json(resp))
}

/// The forecast itself, with **no HTTP anywhere in its signature**: no principal, no `Query`, no
/// `Json`. That is what lets the scheduled sweep ([`crate::forecast_sweep`]) produce the very same
/// `alerts` the handler would, instead of the ETA math only firing for an operator who was already
/// looking. Delivery is the caller's decision — both callers route it through
/// `Alerter::notify_forecast`, which applies the shared cooldown.
pub(crate) async fn compute_forecast(
    st: &AppState,
    project: &str,
    dim: MarginDimension,
    horizon: u32,
    lookback: u32,
) -> Result<ForecastResponse, ApiError> {
    let project = project.to_string();
    let until = Utc::now();
    // The series is `lookback` daily buckets ending today; `start_day` is the oldest bucket's date.
    let start_day = (until - Duration::days((lookback - 1) as i64)).date_naive();
    let since = start_day.and_hms_opt(0, 0, 0).unwrap().and_utc();

    let raw = gather(st, &project, dim, since, until).await?;

    // Dense daily series (gaps → 0) for each metric the budgets might track.
    let cost_series = densify(&by_day(&raw.daily, |d| d.cost_usd), start_day, lookback);
    let calls_series = densify(&by_day(&raw.daily, |d| d.calls as f64), start_day, lookback);
    let tokens_series = densify(
        &by_day(&raw.daily, |d| d.tokens as f64),
        start_day,
        lookback,
    );

    let cost_trend = Trend::fit(&cost_series);
    let spend = SpendProjection {
        projected_daily_cost_usd: round(cost_trend.project(1.0)),
        projected_cost_next_7d_usd: round(cost_trend.project_cumulative(7)),
        projected_cost_next_30d_usd: round(cost_trend.project_cumulative(30)),
        cost_trend,
    };

    let budgets: Vec<BudgetForecast> = raw
        .rules
        .iter()
        .map(|r| {
            let series = match r.metric {
                LimitMetric::CostUsd => &cost_series,
                LimitMetric::Calls => &calls_series,
                LimitMetric::Tokens => &tokens_series,
            };
            let current = raw
                .window_usage
                .get(&r.window)
                .map(|u| u.metric_value(r.metric))
                .unwrap_or(0.0);
            forecast_budget(r, series, current, horizon)
        })
        .collect();

    // Per-dimension daily cost → key → (day → cost), for margin trends.
    let mut dim_by_key: HashMap<String, HashMap<String, f64>> = HashMap::new();
    for d in &raw.daily_dim {
        let key = d.key.clone().unwrap_or_else(|| UNATTRIBUTED.to_string());
        dim_by_key
            .entry(key)
            .or_default()
            .insert(d.day.clone(), d.cost_usd);
    }
    let rows = compute_margin(&raw.revenue, &raw.costs, dim, since, until);
    let margins: Vec<MarginForecast> = rows
        .iter()
        .filter(|row| row.key != UNATTRIBUTED) // unattributed isn't a billable customer/product
        .take(MAX_DIM_FORECASTS)
        .map(|row| {
            let series = dim_by_key
                .get(&row.key)
                .map(|m| densify(m, start_day, lookback))
                .unwrap_or_else(|| vec![0.0; lookback as usize]);
            forecast_margin(
                &row.key,
                row.revenue_usd,
                row.llm_cost_usd,
                &series,
                lookback,
                horizon,
            )
        })
        .collect();

    let alerts = build_alerts(&project, &budgets, &margins);

    Ok(ForecastResponse {
        project_id: project,
        generated_at: until,
        dimension: dim.as_str().to_string(),
        horizon_days: horizon,
        lookback_days: lookback,
        spend,
        budgets,
        margins,
        alerts,
    })
}

/// One blocking hop that reads every series/rollup the forecast needs.
async fn gather(
    st: &AppState,
    project: &str,
    dim: MarginDimension,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<RawForecast, ApiError> {
    let store = st.store.clone();
    let proj = project.to_string();
    let dim_s = dim.as_str().to_string();
    spawn_db(move || {
        let daily = store.daily_usage(&proj, since, until)?;
        let rules = store.list_limit_rules(&proj, true)?;
        let mut window_usage: HashMap<LimitWindow, Usage> = HashMap::new();
        for r in &rules {
            if !window_usage.contains_key(&r.window) {
                window_usage.insert(r.window, store.usage_since(&proj, r.window.since(until))?);
            }
        }
        let revenue = store.list_revenue_events(Some(&proj), since, until)?;
        let costs = store.cost_by_dimension(Some(&proj), &dim_s, since, until)?;
        let daily_dim = store.daily_cost_by_dimension(Some(&proj), &dim_s, since, until)?;
        Ok::<_, StoreError>(RawForecast {
            daily,
            rules,
            window_usage,
            revenue,
            costs,
            daily_dim,
        })
    })
    .await
}

/// Collapse daily rows into a `day → value` map for one metric.
fn by_day(rows: &[DailyUsage], pick: impl Fn(&DailyUsage) -> f64) -> HashMap<String, f64> {
    rows.iter().map(|d| (d.day.clone(), pick(d))).collect()
}

/// Expand a sparse `day → value` map into a dense oldest→newest vector of `days` points starting at
/// `start`, filling absent days with 0 (no traffic that day = no spend).
fn densify(by_day: &HashMap<String, f64>, start: chrono::NaiveDate, days: u32) -> Vec<f64> {
    (0..days)
        .map(|i| {
            let day = (start + Duration::days(i as i64))
                .format("%Y-%m-%d")
                .to_string();
            *by_day.get(&day).unwrap_or(&0.0)
        })
        .collect()
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}
