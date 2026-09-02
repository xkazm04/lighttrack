//! `POST /v1/revenue/reprice` — restate revenue that was stored at the 1:1 FX fallback.
//!
//! `docs/CURRENCY.md` used to answer "a rate was missing when we synced" with "add the rate and
//! re-ingest", which is not a remedy: a Stripe webhook cannot be replayed on demand, and the rows
//! are already someone's recognized revenue. Rows now keep the provider's own minor-unit figure, so
//! the correction can be applied where the mistake is — on the stored row — with a preview first and
//! a book version stamped on whatever it touched.
//!
//! Admin only, and deliberately **not** exposed over MCP: this writes money.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;

use lighttrack_store::RepriceReport;

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

#[derive(Debug, Deserialize)]
pub(crate) struct RepriceParams {
    /// ISO-4217 code to restate, e.g. `GBP`.
    currency: String,
    /// Scope to one project. Absent restates the currency across every project.
    project: Option<String>,
    /// USD per one major unit. Absent takes the shared FX book's current rate for `currency`,
    /// which is the usual case: the operator has just added the missing rate to
    /// `config/fx_rates.json` and restarted.
    rate: Option<f64>,
    /// `1`/`true` (the **default**) reports what would change and writes nothing. Repricing is a
    /// bulk restatement of money; making the destructive form the one you have to ask for is worth
    /// the extra round trip.
    dry_run: Option<String>,
}

/// Whether a query flag reads as set. Mirrors `events_query::is_truthy`.
fn is_truthy(v: Option<&str>) -> bool {
    v.map(str::trim).is_some_and(|s| {
        s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
    })
}

pub(crate) async fn post_reprice(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RepriceParams>,
) -> Result<Json<RepriceReport>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;

    let currency = q.currency.trim().to_uppercase();
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(ApiError::bad_request(format!(
            "`currency` must be a 3-letter ISO-4217 code, got {:?}",
            q.currency
        )));
    }

    let fx = lighttrack_billing::shared_fx();
    let rate = match q.rate {
        Some(r) => r,
        // No explicit rate: take the book's. A currency the book still has no rate for is a 400,
        // not a silent 1:1 — repricing at 1.0 would rewrite every row to the same wrong number it
        // already had while stamping it as converted, which is worse than leaving it flagged.
        None => fx.rate_for(&currency).ok_or_else(|| {
            ApiError::bad_request(format!(
                "no FX rate for {currency} in the current book ({}); add it to config/fx_rates.json \
                 and restart, or pass ?rate=",
                fx.version()
            ))
        })?,
    };
    if !(rate.is_finite() && rate > 0.0) {
        return Err(ApiError::bad_request(format!(
            "`rate` must be a positive finite number, got {rate}"
        )));
    }

    // Absent `dry_run` defaults to a preview. `dry_run=0` is the explicit "yes, write it".
    let dry_run = match q.dry_run.as_deref() {
        None => true,
        Some(v) => is_truthy(Some(v)),
    };

    let version = fx.version().to_string();
    let store = st.store.clone();
    let project = q.project.clone();
    let cur = currency.clone();
    let ver = version.clone();
    let report = spawn_db(move || {
        store.reprice_revenue(project.as_deref().into(), &cur, rate, &ver, dry_run)
    })
    .await?;

    if !report.dry_run && report.changed > 0 {
        // A bulk restatement of recognized revenue is not a debug-level event.
        tracing::warn!(
            currency = %report.currency,
            rate = report.rate,
            book_version = %report.book_version,
            matched = report.matched,
            changed = report.changed,
            project = q.project.as_deref().unwrap_or("(all)"),
            "repriced stored revenue at a corrected FX rate",
        );
    }
    Ok(Json(report))
}
