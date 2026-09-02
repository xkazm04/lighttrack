//! Writing a price, and — opt-in, on the same call — pricing the history that rate was missing for.
//!
//! `PUT /v1/prices/:provider/:model?fill_unpriced=1` is the second half of the M26 loop. A rate
//! landing today has never done anything about yesterday's `cost_usd IS NULL` rows: the operator
//! saw the gap in `/v1/costs/unpriced`, added the price, and the historical numbers stayed a floor
//! forever. The fill closes them.
//!
//! **Opt-in, never automatic.** Filling rewrites stored rows, and a rate added in 2027 is not
//! evidence about what a call cost in 2026 — that judgement is the operator's, so it is a query
//! parameter they type, logged at `info` with the count.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{parse_price_date, ModelPriceRow, PriceBook};
use lighttrack_store::pricing::PriceFill;

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::prices::refresh_book;
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct PutPriceReq {
    input_per_mtok: f64,
    output_per_mtok: f64,
    #[serde(default)]
    cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    source_url: Option<String>,
    /// When this rate takes effect. Defaults to now; a past date backdates the rate on the
    /// timeline, a future one stores it without charging it yet.
    #[serde(default, alias = "effective_date")]
    effective_from: Option<String>,
    /// When a human last checked this rate against the vendor's page.
    #[serde(default)]
    verified_at: Option<String>,
    /// Free-text note — why the rate changed, a ticket id, a caveat.
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PutPriceParams {
    /// `1`/`true` to price the stored `cost_usd IS NULL` rows for this key from the new rate.
    #[serde(default)]
    fill_unpriced: Option<String>,
    /// How far back the "what is left" recount looks (default 90 days).
    #[serde(default)]
    since: Option<String>,
}

/// The stored row, plus what the optional fill did. Flattened, so a client written against the
/// pre-M26 response still reads the row's fields off the top level.
#[derive(Serialize)]
pub(crate) struct PutPriceResponse {
    #[serde(flatten)]
    price: ModelPriceRow,
    /// Rows this call priced. `null` when no fill was asked for — distinct from `0`, which means a
    /// fill ran and found nothing left.
    #[serde(skip_serializing_if = "Option::is_none")]
    filled: Option<u64>,
    /// Calls for this key still carrying no cost, after the fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_unpriced: Option<u64>,
}

fn flag(v: Option<&String>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()),
        Some(ref s) if s == "1" || s == "true" || s == "yes"
    )
}

/// Parse an optional wire date (`YYYY-MM-DD` or RFC3339), refusing anything else rather than
/// silently defaulting: a mis-typed `effective_from` that fell back to "now" would put the rate at
/// the wrong point on a timeline the operator cannot see.
fn opt_date(field: &str, raw: Option<&String>) -> Result<Option<DateTime<Utc>>, ApiError> {
    match raw {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => parse_price_date(s).map(Some).ok_or_else(|| {
            ApiError::bad_request(format!("{field} must be a date or RFC3339 time"))
        }),
    }
}

pub(crate) async fn put_price(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((provider, model)): Path<(String, String)>,
    Query(q): Query<PutPriceParams>,
    Json(req): Json<PutPriceReq>,
) -> Result<Json<PutPriceResponse>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let row = ModelPriceRow {
        provider: provider.clone(),
        model: model.clone(),
        input_per_mtok: req.input_per_mtok,
        output_per_mtok: req.output_per_mtok,
        cached_input_per_mtok: req.cached_input_per_mtok,
        effective_from: opt_date("effective_from", req.effective_from.as_ref())?
            .unwrap_or_else(Utc::now),
        source_url: req.source_url,
        verified_at: opt_date("verified_at", req.verified_at.as_ref())?,
        note: req.note,
    };
    let store = st.store.clone();
    let row2 = row.clone();
    spawn_db(move || store.upsert_price(&row2)).await?;

    // Hot-swap the in-memory book so new prices take effect without a restart — and so the fill
    // below prices from the book the rate just joined, not from a stale snapshot.
    let fresh = refresh_book(&st).await?;

    if !flag(q.fill_unpriced.as_ref()) {
        return Ok(Json(PutPriceResponse {
            price: row,
            filled: None,
            remaining_unpriced: None,
        }));
    }

    let since =
        opt_date("since", q.since.as_ref())?.unwrap_or_else(|| Utc::now() - Duration::days(90));
    let (p, m) = (provider.clone(), model.clone());
    let store = st.store.clone();
    let (filled, remaining) =
        spawn_db(move || fill_and_recount(&*store, &fresh, &p, &m, since)).await?;
    tracing::info!(
        provider = %provider, model = %model, filled, remaining_unpriced = remaining,
        "priced stored rows from a newly-added rate (fill_unpriced)"
    );
    Ok(Json(PutPriceResponse {
        price: row,
        filled: Some(filled),
        remaining_unpriced: Some(remaining),
    }))
}

/// Fill, then re-read the ledger for this key: `remaining_unpriced` has to be *measured*, not
/// inferred from the fill count. A row the book still cannot price (a variant rate that leaves the
/// base model unpriced) is exactly the case where "filled 3" would otherwise read as "done".
fn fill_and_recount(
    store: &dyn lighttrack_store::Store,
    book: &PriceBook,
    provider: &str,
    model: &str,
    since: DateTime<Utc>,
) -> lighttrack_store::Result<(u64, u64)> {
    let filled = store.fill_unpriced_cost(&PriceFill::new(provider, model, book))?;
    let remaining = store
        .list_unpriced(None, since)?
        .into_iter()
        .filter(|r| r.provider == provider && r.model == model)
        .map(|r| r.calls)
        .sum();
    Ok((filled, remaining))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fill_flag_is_opt_in_and_only_true_for_an_affirmative() {
        for yes in ["1", "true", "TRUE", "yes"] {
            assert!(flag(Some(&yes.to_string())), "{yes}");
        }
        for no in ["0", "false", "", "maybe"] {
            assert!(!flag(Some(&no.to_string())), "{no}");
        }
        assert!(!flag(None), "absent means no — a fill is never implicit");
    }

    /// A mistyped date must 400, not fall back to `now`: silently landing a rate at the wrong point
    /// on the timeline is worse than refusing the write.
    #[test]
    fn a_malformed_date_is_refused_rather_than_defaulted() {
        let parsed =
            |s: Option<&str>| match opt_date("effective_from", s.map(str::to_string).as_ref()) {
                Ok(v) => Ok(v.is_some()),
                Err(_) => Err(()),
            };
        assert_eq!(parsed(Some("2026-05-31")), Ok(true));
        assert_eq!(parsed(Some("2026-05-31T00:00:00Z")), Ok(true));
        assert_eq!(parsed(None), Ok(false), "absent is not an error");
        assert_eq!(parsed(Some("  ")), Ok(false), "blank is not an error");
        assert_eq!(parsed(Some("soon")), Err(()), "nonsense is refused");
    }
}
