//! One row of the dated price book, and how fresh the book as a whole is.
//!
//! Split out of [`crate::pricing`] because the row is a *stored* type with its own wire contract
//! (M26 renamed its date column and added two fields), while [`crate::pricing::PriceBook`] is the
//! in-memory index built over many rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::pricing::PriceBook;

/// A persisted price-book row (the DB-backed source of truth; `pricing.json` is just the seed).
///
/// The book is **append-only and dated** (M26): a row's identity is
/// `(provider, model, effective_from)`, so correcting a rate adds a row instead of overwriting the
/// one that priced last quarter's traffic, and `GET /v1/prices/history/:provider/:model` can show
/// the timeline. `verified_at` is when a human last checked the rate against the vendor's page —
/// the seed's `_meta.last_verified` used to be invisible at runtime, which is how a two-year-old
/// price book looks exactly like a freshly-checked one.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelPriceRow {
    pub provider: String,
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    /// When this rate took effect. `effective_date` is accepted as the pre-M26 spelling, so a
    /// client written against the old wire shape still parses.
    #[serde(default = "Utc::now", alias = "effective_date")]
    pub effective_from: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// When this rate was last checked against the vendor's published pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<DateTime<Utc>>,
    /// Free-text operator note — why the rate changed, a ticket id, a caveat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ModelPriceRow {
    /// The book key this row prices (`"<provider>/<model>"`).
    pub fn key(&self) -> String {
        PriceBook::key(&self.provider, &self.model)
    }
}

/// Default staleness budget for [`PriceBookPosture`], overridable by `LIGHTTRACK_PRICE_STALE_DAYS`.
pub const DEFAULT_PRICE_STALE_DAYS: i64 = 60;

/// How trustworthy the price book is, reported at boot and beside the cost reads it feeds.
///
/// A cost dashboard computed from rates nobody has checked in two years is not wrong-*looking*; it
/// is confidently wrong, which is worse. `stale` is the one bit an operator needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceBookPosture {
    /// The **oldest** `verified_at` across the book's current rows — the book is only as fresh as
    /// its weakest row — or `None` when no row carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<DateTime<Utc>>,
    pub stale: bool,
    /// The staleness budget, in days, this verdict was measured against.
    pub stale_after_days: i64,
    /// How many current rows were judged.
    pub rows: usize,
}

impl PriceBookPosture {
    /// Judge a set of current rows at `now` against a `stale_after_days` budget.
    pub fn measure(rows: &[ModelPriceRow], now: DateTime<Utc>, stale_after_days: i64) -> Self {
        let verified_at = rows.iter().filter_map(|r| r.verified_at).min();
        // A book where nothing carries a `verified_at` is not fresh — it is a book nobody has ever
        // vouched for, which is exactly the state this was built to stop hiding.
        let stale = match verified_at {
            Some(v) => (now - v).num_days() > stale_after_days,
            None => !rows.is_empty(),
        };
        PriceBookPosture {
            verified_at,
            stale,
            stale_after_days,
            rows: rows.len(),
        }
    }

    /// The staleness budget from `LIGHTTRACK_PRICE_STALE_DAYS`, or the default.
    pub fn budget_from_env() -> i64 {
        std::env::var("LIGHTTRACK_PRICE_STALE_DAYS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_PRICE_STALE_DAYS)
    }
}

/// Parse a date that may be a bare `YYYY-MM-DD` (what `pricing.json`'s `_meta` writes) or a full
/// RFC3339 timestamp.
pub fn parse_price_date(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s.trim()) {
        return Some(d.with_timezone(&Utc));
    }
    let d = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()?;
    Some(DateTime::from_naive_utc_and_offset(
        d.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, verified: Option<&str>) -> ModelPriceRow {
        ModelPriceRow {
            provider: "openai".into(),
            model: model.into(),
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cached_input_per_mtok: None,
            effective_from: parse_price_date("2026-01-01").expect("date"),
            source_url: None,
            verified_at: verified.and_then(parse_price_date),
            note: None,
        }
    }

    #[test]
    fn both_date_spellings_parse_and_anything_else_refuses() {
        assert!(parse_price_date("2026-05-31").is_some());
        assert!(parse_price_date("2026-05-31T12:00:00Z").is_some());
        assert!(parse_price_date("last tuesday").is_none());
        assert!(parse_price_date("").is_none());
    }

    #[test]
    fn posture_reports_the_oldest_verified_row() {
        let now = parse_price_date("2026-09-01").expect("date");
        let fresh = row("gpt-9", Some("2026-08-20"));
        let old = row("gpt-8", Some("2026-01-02"));

        let p = PriceBookPosture::measure(std::slice::from_ref(&fresh), now, 60);
        assert!(!p.stale);
        assert_eq!(p.rows, 1);

        let p = PriceBookPosture::measure(&[fresh, old], now, 60);
        assert!(p.stale, "the book is only as fresh as its oldest row");
        assert_eq!(p.verified_at, parse_price_date("2026-01-02"));

        assert!(
            PriceBookPosture::measure(&[row("gpt-7", None)], now, 60).stale,
            "a book nobody vouched for is stale, not fresh"
        );
        assert!(
            !PriceBookPosture::measure(&[], now, 60).stale,
            "an empty book has nothing to be stale about"
        );
    }

    /// The pre-M26 wire spelling still parses — an older client's `PUT` body must not 400.
    #[test]
    fn effective_date_is_accepted_as_the_legacy_spelling() {
        let r: ModelPriceRow = serde_json::from_str(
            r#"{"provider":"openai","model":"gpt-9","input_per_mtok":1.0,
                "output_per_mtok":2.0,"effective_date":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("legacy row parses");
        assert_eq!(r.effective_from, parse_price_date("2026-01-01").expect("d"));
        assert_eq!(r.verified_at, None);
        assert_eq!(r.key(), "openai/gpt-9");
    }
}
