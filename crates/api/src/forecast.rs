//! Predictive cost/margin forecasting surface.
//!
//! Turns the rolling daily counters the system already keeps into a forward look and pre-emptive
//! alerts: *which budgets are on track to breach, and when?* and *which customers/products are
//! trending unprofitable?* The math is pure ([`lighttrack_core::forecast`]); this module is wiring —
//! pull the daily series + limits + revenue, project, shape JSON, and fire best-effort alerts.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::forecast::{forecast_budget, forecast_margin, BudgetForecast, MarginForecast};
use lighttrack_core::forecast_gate::{MIN_OBSERVED_DAYS, MIN_SPAN_DAYS};
use lighttrack_core::margin::UNATTRIBUTED;
use lighttrack_core::{
    compute_margin, CostByDimension, LimitMetric, LimitRule, LimitWindow, MarginDimension,
    MarginRow, Project, RevenueEvent, Trend,
};
use lighttrack_store::{DailyDimCost, DailyUsage, StoreError, Usage};

use crate::error::ApiError;
use crate::forecast_alerts::{build_alerts, ForecastAlert};
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

/// At most this many customers/products are forecast (the worst-margin ones first), to bound the
/// response and the per-key trend work.
const MAX_DIM_FORECASTS: usize = 50;

/// Shortest lookback a caller may ask for. Below the evidence floor a trend cannot be presented at
/// all ([`lighttrack_core::forecast_gate`]), so accepting `lookback=2` would only mean answering
/// every projection with a refusal — clamping says the same thing without pretending to try.
const MIN_LOOKBACK_DAYS: u32 = MIN_OBSERVED_DAYS as u32;

#[derive(Deserialize)]
pub(crate) struct ForecastParams {
    project: Option<String>,
    /// `customer` (default) | `product` — the billing axis for margin forecasting.
    by: Option<String>,
    /// How far ahead to project, in days (default 14, clamped to 1..=90).
    horizon: Option<u32>,
    /// How many trailing days of history to fit the trend over (default 14, clamped to 4..=90).
    lookback: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct SpendProjection {
    cost_trend: Trend,
    projected_daily_cost_usd: f64,
    projected_cost_next_7d_usd: f64,
    projected_cost_next_30d_usd: f64,
    /// r² of the spend fit, or `null` when the trend is under the evidence floor — in which case
    /// the matching entry in `refused[]` says what is missing.
    confidence: Option<f64>,
}

/// One projection this response declines to make, and why. A forecast surface that answers a
/// too-young project with silence is indistinguishable from one answering "all is well"; naming the
/// refusal is what keeps the difference visible.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Refused {
    /// `spend`, a limit rule id, or a customer/product key.
    pub(crate) subject: String,
    pub(crate) reason: String,
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
    pub(crate) margins: Vec<MarginForecast>,
    /// Pre-emptive warnings derived from the forecasts (also delivered best-effort to alert sinks,
    /// by the handler and by the scheduled sweep alike).
    pub(crate) alerts: Vec<ForecastAlert>,
    /// Projections withheld because the history behind them is too thin to mean anything. Always
    /// present (possibly empty), so an operator — and the escalation pass — can tell "no risk" from
    /// "no evidence".
    pub(crate) refused: Vec<Refused>,
    /// The windowed margin rows the `margins` forecasts were built from. Not serialized — the
    /// `/v1/margin` surface is where an operator reads these — but carried so the guardrail pass
    /// acts on exactly the numbers this forecast was computed from, rather than re-reading the
    /// window and possibly deciding against a slightly different picture.
    #[serde(skip)]
    pub(crate) margin_rows: Vec<MarginRow>,
}

/// Raw store reads gathered in one blocking hop, before any pure shaping.
struct RawForecast {
    /// The tenant row, for its `created_at` — half of the definition seam under `densify`.
    project: Option<Project>,
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
    let lookback = q.lookback.unwrap_or(14).clamp(MIN_LOOKBACK_DAYS, 90);

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
    // Both callers are clamped here rather than only at the handler, so the scheduled sweep cannot
    // be configured into a lookback the evidence floor would refuse anyway.
    let lookback = lookback.clamp(MIN_LOOKBACK_DAYS, 90);
    let until = Utc::now();
    // The series is `lookback` daily buckets ending today; `start_day` is the oldest bucket's date.
    let start_day = (until - Duration::days((lookback - 1) as i64)).date_naive();
    let since = start_day.and_hms_opt(0, 0, 0).unwrap().and_utc();

    let raw = gather(st, &project, dim, since, until).await?;
    let first = first_observed(raw.project.as_ref(), &raw.daily, start_day);

    // Dense daily series for each metric the budgets might track. Gaps become 0 — but only from the
    // project's first observed day onward (see `first_observed`).
    let cost_series = densify(
        &by_day(&raw.daily, |d| d.cost_usd),
        start_day,
        lookback,
        first,
    );
    let calls_series = densify(
        &by_day(&raw.daily, |d| d.calls as f64),
        start_day,
        lookback,
        first,
    );
    let tokens_series = densify(
        &by_day(&raw.daily, |d| d.tokens as f64),
        start_day,
        lookback,
        first,
    );

    let mut refused: Vec<Refused> = Vec::new();
    let cost_trend = Trend::fit(&cost_series);
    if let Err(r) = cost_trend.presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS) {
        refused.push(Refused {
            subject: "spend".into(),
            reason: r.reason,
        });
    }
    let spend = SpendProjection {
        projected_daily_cost_usd: round(cost_trend.project(1.0)),
        projected_cost_next_7d_usd: round(cost_trend.project_cumulative(7)),
        projected_cost_next_30d_usd: round(cost_trend.project_cumulative(30)),
        confidence: cost_trend.r2,
        cost_trend,
    };

    // A revenue-share rule has no fixed figure to cross: its cap resolves per customer against that
    // customer's recognized revenue, and the daily series here is the project's. Forecasting it
    // against `nominal_threshold()` (infinity for a derived threshold) used to publish a row with
    // `threshold: null`, no ETA and no refusal — the one shape this surface promises never to emit.
    let mut budgets: Vec<BudgetForecast> = raw
        .rules
        .iter()
        .filter(|r| {
            if r.threshold.fixed().is_some() {
                return true;
            }
            refused.push(Refused {
                subject: r.id.clone(),
                reason: "revenue-share threshold resolves per customer; not forecast from the                          project's daily series"
                    .into(),
            });
            false
        })
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
    let mut margins: Vec<MarginForecast> = rows
        .iter()
        .filter(|row| row.key != UNATTRIBUTED) // unattributed isn't a billable customer/product
        .take(MAX_DIM_FORECASTS)
        .map(|row| {
            let series = dim_by_key
                .get(&row.key)
                .map(|m| densify(m, start_day, lookback, first))
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

    // Withhold every ETA the evidence floor refuses, and say so. Nulling the field rather than
    // leaving a number nobody may act on is the point: the JSON must not carry a projection the
    // alert path has already decided is unsayable.
    for b in &mut budgets {
        if let Err(r) = b.trend.presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS) {
            b.eta_days = None;
            refused.push(Refused {
                subject: b.rule_id.clone(),
                reason: r.reason,
            });
        }
    }
    for m in &mut margins {
        if let Err(r) = m
            .cost_trend
            .presentability(MIN_OBSERVED_DAYS, MIN_SPAN_DAYS)
        {
            m.eta_unprofitable_days = None;
            refused.push(Refused {
                subject: m.key.clone(),
                reason: r.reason,
            });
        }
    }

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
        refused,
        margin_rows: rows,
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
        let project = store.get_project(&proj)?;
        let daily = store.daily_usage(&proj, since, until)?;
        let rules = store.list_limit_rules(&proj, true)?;
        let mut window_usage: HashMap<LimitWindow, Usage> = HashMap::new();
        for r in &rules {
            // Vacant-entry form rather than `or_insert_with`: the value is a fallible store call and
            // the closure could not propagate `?`.
            if let Entry::Vacant(e) = window_usage.entry(r.window) {
                e.insert(store.usage_since(&proj, r.window.since(until))?);
            }
        }
        let revenue = store.list_revenue_events(TenantScope::Project(&proj), since, until)?;
        let costs = store.cost_by_dimension(TenantScope::Project(&proj), &dim_s, since, until)?;
        let daily_dim =
            store.daily_cost_by_dimension(TenantScope::Project(&proj), &dim_s, since, until)?;
        Ok::<_, StoreError>(RawForecast {
            project,
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

/// The **definition seam**: the first day in this window on which "no traffic" is a real
/// observation rather than an absence of history. Days before it are dropped from the fitted series
/// instead of being zero-filled.
///
/// A project that predates the window (`created_at <= start`) could have spent on any day in it, so
/// every quiet day inside is a genuine zero and nothing is trimmed (`None`). A project created
/// *inside* the window has its whole life in view, so the first day it actually spent is the first
/// day there was anything to observe — the eleven zeros in front of it are the calendar, not the
/// project, and fitting a slope through them is how "started spending on Tuesday" became "on track
/// to breach".
fn first_observed(
    project: Option<&Project>,
    daily: &[DailyUsage],
    start: chrono::NaiveDate,
) -> Option<chrono::NaiveDate> {
    let created = project?.created_at.date_naive();
    if created <= start {
        return None;
    }
    let first_traffic = daily
        .iter()
        .filter_map(|d| chrono::NaiveDate::parse_from_str(&d.day, "%Y-%m-%d").ok())
        .min();
    // The *earliest* of the two, not the later: traffic is itself proof the project existed, so a
    // row whose `created_at` was backfilled after the fact must not erase history that plainly
    // happened. The seam is "first evidence of existence", and evidence beats bookkeeping.
    Some(first_traffic.map_or(created, |t| t.min(created)))
}

/// Expand a sparse `day → value` map into a dense oldest→newest vector starting at `start`, filling
/// absent days with 0 (no traffic that day = no spend). Days before `first_observed` are **omitted**
/// rather than zero-filled: see [`first_observed`] for why the difference is the whole point.
fn densify(
    by_day: &HashMap<String, f64>,
    start: chrono::NaiveDate,
    days: u32,
    first_observed: Option<chrono::NaiveDate>,
) -> Vec<f64> {
    (0..days)
        .map(|i| start + Duration::days(i as i64))
        .filter(|d| first_observed.is_none_or(|f| *d >= f))
        .map(|d| {
            let day = d.format("%Y-%m-%d").to_string();
            *by_day.get(&day).unwrap_or(&0.0)
        })
        .collect()
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}
