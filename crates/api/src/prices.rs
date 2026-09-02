//! Model prices (Phase 3.6a) — DB-backed, hot-swappable.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;

use lighttrack_core::{AliasTable, ModelPriceRow, PriceBook, PriceBookPosture};

use crate::error::ApiError;
use crate::guards::authenticate;
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
        // Absent is the normal binary-only install and needs no comment. Present-but-unreadable (a
        // permissions mistake, a directory at that path) is an operator's edited book being silently
        // ignored — the exact failure the boot-time "source" field exists to expose.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (embedded(), PriceSeed::Embedded),
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "pricing file exists but could not be read; using the compiled-in price book");
            (embedded(), PriceSeed::Embedded)
        }
    }
}

/// The declared model aliases from the seed, re-attached to every book built from DB rows.
///
/// `model_prices` has no alias column (M8 changes no schema) and none is wanted: an alias is a
/// statement about *identity*, which ships with the build, while the DB is the source of truth for
/// *prices*. Taken from the compiled-in seed so the table is identical on every instance of a
/// release, however the operator edited their local `pricing.json` rates.
pub(crate) fn declared_aliases() -> AliasTable {
    static TABLE: std::sync::OnceLock<AliasTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| embedded().aliases().clone()).clone()
}

fn embedded() -> PriceBook {
    // A malformed embedded book is a build-time mistake, not a runtime condition; the test below
    // makes that a compile-and-test failure rather than a silent empty book in production.
    PriceBook::from_json_str(EMBEDDED_PRICING).unwrap_or_default()
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

/// One key's price timeline, newest first — `GET /v1/prices/history/:provider/:model`.
///
/// `GET /v1/prices` answers "what are we charging now". This answers "what were we charging in
/// June", which is the only thing a June cost number can be defended with now that the book is
/// dated and append-only (M26).
pub(crate) async fn get_price_history(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((provider, model)): Path<(String, String)>,
) -> Result<Json<Vec<ModelPriceRow>>, ApiError> {
    authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_price_history(&provider, &model)).await?;
    Ok(Json(rows))
}

/// Re-read the book from the store and hot-swap the in-memory copy, returning the new book.
///
/// Build outside the lock, swap under it: the critical section is one pointer-sized assignment, and
/// a poisoned lock is recovered rather than propagated (see `events::prepare_event`).
pub(crate) async fn refresh_book(st: &AppState) -> Result<PriceBook, ApiError> {
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_prices()).await?;
    let fresh = PriceBook::from_rows(&rows).with_aliases(declared_aliases());
    {
        let mut book = st.prices.write().unwrap_or_else(|p| p.into_inner());
        *book = fresh.clone();
    }
    Ok(fresh)
}

/// How fresh the stored book is, measured against `LIGHTTRACK_PRICE_STALE_DAYS` (default 60).
///
/// Read from the store rather than the in-memory book, because `verified_at` is a property of the
/// *rows*: the in-memory `PriceBook` is an index of rates, and the question here is who last
/// vouched for them.
pub(crate) fn measure_posture(rows: &[ModelPriceRow]) -> PriceBookPosture {
    PriceBookPosture::measure(rows, Utc::now(), PriceBookPosture::budget_from_env())
}

/// The boot-time staleness line. Its own log record at `warn`, not a field on the startup posture
/// event: a price book nobody has checked in months makes every cost, margin and limit number
/// quietly wrong, and that does not belong buried in a field list.
pub(crate) fn log_price_posture(p: &PriceBookPosture) {
    if !p.stale {
        return;
    }
    match p.verified_at {
        Some(v) => tracing::warn!(
            verified_at = %v, stale_after_days = p.stale_after_days, rows = p.rows,
            "the model price book has not been verified recently — every cost, margin and limit \
             number is computed from these rates; re-check them against the vendors' pricing pages"
        ),
        None => tracing::warn!(
            rows = p.rows,
            "no row in the model price book carries a verification date — costs are computed from \
             rates nobody has vouched for; set `verified_at` when you next confirm them"
        ),
    }
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

    /// The seed's `_meta.last_verified` has to survive into the rows the API stores, or the boot
    /// warning measures nothing on a fresh install.
    #[test]
    fn the_seeded_rows_carry_the_books_verification_date() {
        let (book, _) = seed_book("no/such/pricing.json");
        let rows = book.rows();
        let at = book.verified_at().expect("the seed declares last_verified");
        assert!(rows.iter().all(|r| r.verified_at == Some(at)));
        // …and a book nobody has re-checked in over a year is judged stale rather than trusted.
        let p = PriceBookPosture::measure(&rows, at + chrono::Duration::days(400), 60);
        assert!(p.stale);
        assert_eq!(p.rows, rows.len());
    }
}
