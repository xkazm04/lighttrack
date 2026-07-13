//! Revenue ingest + the profit/margin rollup. Cost is reused from the LLM event stream (the price
//! book prices every provider); revenue is netted against it per customer/product over a window.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{compute_margin, MarginDimension, MarginRow, RevenueEvent};

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

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| ApiError::bad_request(format!("invalid RFC3339 timestamp: {s}")))
}

fn round(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}
