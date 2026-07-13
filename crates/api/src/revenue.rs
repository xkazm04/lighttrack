//! Revenue ingest + the profit/margin rollup. Cost is reused from the LLM event stream (the price
//! book prices every provider); revenue is netted against it per customer/product over a window.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::margin::UNATTRIBUTED;
use lighttrack_core::{
    compute_margin, compute_margin_trend, DailyKeyCost, MarginDimension, MarginRow,
    MarginTrendSeries, RevenueEvent,
};
use lighttrack_store::DailyDimCost;

use crate::error::ApiError;
use crate::guards::{authenticate, resolve_ingest_project, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// Post one revenue record (manual, or from a future Stripe/Polar sync). Project is derived from the
/// key, mirroring event ingest.
pub(crate) async fn post_revenue(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut ev): Json<RevenueEvent>,
) -> Result<Json<RevenueEvent>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    ev.project_id = resolve_ingest_project(&principal, &ev.project_id)?;
    let store = st.store.clone();
    let to_insert = ev.clone();
    spawn_db(move || store.insert_revenue_event(&to_insert)).await?;
    Ok(Json(ev))
}

#[derive(Deserialize)]
pub(crate) struct MarginParams {
    project: Option<String>,
    /// `customer` (default) | `product`.
    by: Option<String>,
    /// RFC3339 window bounds; default to the last 30 days.
    since: Option<String>,
    until: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct MarginResponse {
    dimension: String,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    total_revenue_usd: f64,
    total_cost_usd: f64,
    total_margin_usd: f64,
    /// Currencies in this window's revenue that had no FX rate and were stored at 1:1 — the USD
    /// figures above are approximate for them. Empty (omitted) when every currency was convertible.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unconverted_currencies: Vec<String>,
    /// Human-facing note when `unconverted_currencies` is non-empty (rendered as a caveat).
    #[serde(skip_serializing_if = "Option::is_none")]
    currency_note: Option<String>,
    rows: Vec<MarginRow>,
}

/// Distinct non-convertible currencies present in `revenue` (per the shared FX table): non-USD codes
/// with no rate, whose `amount_usd` was a 1:1 fallback. Sorted, deduped, for a stable health note.
fn unconverted_currencies(revenue: &[RevenueEvent]) -> Vec<String> {
    let fx = lighttrack_billing::shared_fx();
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in revenue {
        if !fx.is_convertible(&r.currency) {
            set.insert(r.currency.to_uppercase());
        }
    }
    set.into_iter().collect()
}

pub(crate) async fn get_margin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MarginParams>,
) -> Result<Json<MarginResponse>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let dim = MarginDimension::parse(q.by.as_deref().unwrap_or("customer"));

    let until = match q.until.as_deref() {
        Some(s) => parse_rfc3339(s)?,
        None => Utc::now(),
    };
    let since = match q.since.as_deref() {
        Some(s) => parse_rfc3339(s)?,
        None => until - Duration::days(30),
    };
    if since >= until {
        return Err(ApiError::bad_request("`since` must be before `until`"));
    }

    let store = st.store.clone();
    let proj = project.clone();
    let revenue = spawn_db(move || store.list_revenue_events(proj.as_deref(), since, until)).await?;

    let store = st.store.clone();
    let proj = project.clone();
    let dim_s = dim.as_str().to_string();
    let costs =
        spawn_db(move || store.cost_by_dimension(proj.as_deref(), &dim_s, since, until)).await?;

    let unconverted = unconverted_currencies(&revenue);

    let rows = compute_margin(&revenue, &costs, dim, since, until);
    let total_revenue_usd: f64 = rows.iter().map(|r| r.revenue_usd).sum();
    let total_cost_usd: f64 = rows.iter().map(|r| r.llm_cost_usd).sum();
    let currency_note = (!unconverted.is_empty()).then(|| {
        format!(
            "unconverted currencies present (stored 1:1, USD figures approximate): {}. \
             Add rates to config/fx_rates.json.",
            unconverted.join(", ")
        )
    });
    Ok(Json(MarginResponse {
        dimension: dim.as_str().to_string(),
        since,
        until,
        total_revenue_usd: round(total_revenue_usd),
        total_cost_usd: round(total_cost_usd),
        total_margin_usd: round(total_revenue_usd - total_cost_usd),
        unconverted_currencies: unconverted,
        currency_note,
        rows,
    }))
}

// --- margin trend (per-day series per dimension key) --------------------------------------------

/// Hard cap on the trend window, to bound the O(revenue × days) recognition work and the response.
const MAX_TREND_DAYS: u32 = 365;
/// Default top-N keys (by |margin|) when `?top=` is absent and `LIGHTTRACK_MARGIN_TREND_TOP_N` unset.
const DEFAULT_TREND_TOP_N: usize = 20;

#[derive(Deserialize)]
pub(crate) struct MarginTrendParams {
    project: Option<String>,
    /// `customer` (default) | `product`.
    by: Option<String>,
    /// Trailing window length in days (default 30, clamped to 1..=365).
    days: Option<u32>,
    /// Max keys returned, by |total margin| (default `LIGHTTRACK_MARGIN_TREND_TOP_N` or 20).
    top: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct MarginTrendResponse {
    dimension: String,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    days: u32,
    /// Distinct dimension keys before the top-N cap (so the client can say "showing N of key_count").
    key_count: usize,
    top_n: usize,
    /// All-keys per-day totals (complete, not capped).
    totals: MarginTrendSeries,
    /// The top-N keys' dense daily series.
    series: Vec<MarginTrendSeries>,
}

/// `GET /v1/margin/trend` — per-day revenue/cost/margin per customer or product over a trailing window.
/// Revenue is recognized per day by the same rules as `/v1/margin`; cost from the per-day dimension
/// rollup. Only the SQLite backend fills the daily cost series today; Postgres/Firestore return an
/// empty cost series (revenue-only trend) until they port `daily_cost_by_dimension`.
pub(crate) async fn get_margin_trend(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MarginTrendParams>,
) -> Result<Json<MarginTrendResponse>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let dim = MarginDimension::parse(q.by.as_deref().unwrap_or("customer"));
    let days = q.days.unwrap_or(30).clamp(1, MAX_TREND_DAYS);
    let top_n = q.top.unwrap_or_else(default_top_n).max(1);

    let until = Utc::now();
    // `days` UTC calendar days ending today; `start_day` is the oldest bucket's date.
    let start_day = (until - Duration::days((days - 1) as i64)).date_naive();
    let since = start_day.and_hms_opt(0, 0, 0).unwrap().and_utc();

    let store = st.store.clone();
    let proj = project.clone();
    let dim_s = dim.as_str().to_string();
    let (revenue, daily): (Vec<RevenueEvent>, Vec<DailyDimCost>) = spawn_db(move || {
        let revenue = store.list_revenue_events(proj.as_deref(), since, until)?;
        let daily = store.daily_cost_by_dimension(proj.as_deref(), &dim_s, since, until)?;
        Ok::<_, lighttrack_store::StoreError>((revenue, daily))
    })
    .await?;

    let daily_cost: Vec<DailyKeyCost> = daily
        .into_iter()
        .map(|d| DailyKeyCost {
            day: d.day,
            key: d.key.unwrap_or_else(|| UNATTRIBUTED.to_string()),
            cost_usd: d.cost_usd,
        })
        .collect();

    let trend = compute_margin_trend(&revenue, &daily_cost, dim, start_day, days, top_n);
    Ok(Json(MarginTrendResponse {
        dimension: dim.as_str().to_string(),
        since,
        until,
        days,
        key_count: trend.key_count,
        top_n: trend.top_n,
        totals: trend.totals,
        series: trend.series,
    }))
}

fn default_top_n() -> usize {
    std::env::var("LIGHTTRACK_MARGIN_TREND_TOP_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TREND_TOP_N)
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| ApiError::bad_request(format!("invalid RFC3339 timestamp: {s}")))
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}
