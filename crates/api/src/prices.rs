//! Model prices (Phase 3.6a) — DB-backed, hot-swappable.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{ModelPriceRow, PriceBook};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// The price book shipped with the source tree, compiled in. Release archives carry only the
/// binaries, so a `curl … | sh` install has no `config/pricing.json` next to it — without this
/// fallback such an instance seeds an empty book and prices every event at `null`, silently, which
/// is indistinguishable from "this model is free". A file on disk still wins when there is one.
const EMBEDDED_PRICING: &str = include_str!("../../../config/pricing.json");

/// Where a startup price book came from — reported at boot so an operator can tell whether their
/// edits to `pricing.json` were actually picked up.
pub(crate) enum PriceSeed {
    File,
    Embedded,
}

/// Build the seed price book: `path` if it parses, else the compiled-in copy.
pub(crate) fn seed_book(path: &str) -> (PriceBook, PriceSeed) {
    match std::fs::read_to_string(path) {
        Ok(s) => match PriceBook::from_json_str(&s) {
            Ok(b) => (b, PriceSeed::File),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "pricing file did not parse; using the compiled-in price book");
                (embedded(), PriceSeed::Embedded)
            }
        },
        Err(_) => (embedded(), PriceSeed::Embedded),
    }
}

fn embedded() -> PriceBook {
    // A malformed embedded book is a build-time mistake, not a runtime condition; the test below
    // makes that a compile-and-test failure rather than a silent empty book in production.
    PriceBook::from_json_str(EMBEDDED_PRICING).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_price_book_parses_and_is_not_empty() {
        let b =
            PriceBook::from_json_str(EMBEDDED_PRICING).expect("embedded pricing.json must parse");
        assert!(!b.is_empty(), "embedded price book seeded no models");
    }

    #[test]
    fn a_missing_pricing_file_falls_back_to_the_embedded_book_not_an_empty_one() {
        let (book, seed) = seed_book("no/such/pricing.json");
        assert!(matches!(seed, PriceSeed::Embedded));
        assert_eq!(book.len(), embedded().len());
        assert!(
            !book.is_empty(),
            "a binary-only install must still price events"
        );
    }
}

pub(crate) async fn get_prices(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ModelPriceRow>>, ApiError> {
    authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_prices()).await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub(crate) struct PutPriceReq {
    input_per_mtok: f64,
    output_per_mtok: f64,
    #[serde(default)]
    cached_input_per_mtok: Option<f64>,
    #[serde(default)]
    source_url: Option<String>,
}

pub(crate) async fn put_price(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((provider, model)): Path<(String, String)>,
    Json(req): Json<PutPriceReq>,
) -> Result<Json<ModelPriceRow>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let row = ModelPriceRow {
        provider,
        model,
        input_per_mtok: req.input_per_mtok,
        output_per_mtok: req.output_per_mtok,
        cached_input_per_mtok: req.cached_input_per_mtok,
        effective_date: Utc::now(),
        source_url: req.source_url,
    };
    let store = st.store.clone();
    let row2 = row.clone();
    spawn_db(move || store.upsert_price(&row2)).await?;

    // Hot-swap the in-memory price book so new prices take effect without a restart.
    let store2 = st.store.clone();
    let rows = spawn_db(move || store2.list_prices()).await?;
    // Build outside the lock, swap under it: the critical section is one pointer-sized assignment,
    // and a poisoned lock is recovered rather than propagated (see `events::prepare_event`).
    let fresh = PriceBook::from_rows(&rows);
    {
        let mut book = st.prices.write().unwrap_or_else(|p| p.into_inner());
        *book = fresh;
    }
    Ok(Json(row))
}
