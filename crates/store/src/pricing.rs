//! The unpriced ledger and the forward fill: the two halves of "see the gap → add the price → the
//! numbers become honest".
//!
//! [`list_unpriced_via_rollup`] is deliberately not a new per-backend query. The M2 rollup already
//! answers "usage over a window grouped by 1..3 dimensions", and M26 gave it one extra predicate
//! ([`RollupQuery::unpriced_only`]); grouping by `provider × model × day` and folding the days in
//! Rust yields the whole ledger — call counts, token sums, and a first/last-seen day — for every
//! backend that serves `Surface::Rollup`, with the unpriced predicate written exactly once per
//! backend instead of four times.
//!
//! [`FILL_SOURCE`] is the other invariant: a filled row is *labelled* filled. Cost provenance is
//! already a three-way distinction (`client` — the caller's own number; `book` — our arithmetic at
//! ingest; absent — unpriced), and a backfill that stamped `book` would make a price applied
//! retroactively indistinguishable from one that was in force at the time.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use lighttrack_core::{
    Dimension, ModelPriceRow, PriceBook, PricingMode, RollupQuery, TimeKey, TokenUsage, UnpricedRow,
};

use crate::{Result, Store};

/// `metadata.cost_source` for a row priced after the fact by [`Store::fill_unpriced_cost`].
///
/// Distinct from `book` on purpose: `book` means "the price was in the book when the call
/// happened". Collapsing the two would erase the one fact an auditor needs — that this number was
/// reconstructed later, from a rate that may not have been the rate in force.
pub const FILL_SOURCE: &str = "book_fill";

/// What a forward fill needs: which key to price, the book to price it from, and the stamp.
///
/// The whole [`PriceBook`] rather than a single `ModelPrice` because pricing is not one
/// multiplication: prompt-length tiers (`@in>N`) and batch/flex lanes are variant rows, so a row's
/// rate depends on its own token count and lane. Resolving that in Rust — per row, through the same
/// [`PriceBook::cost_usd_mode`] ingest uses — is what keeps a filled row equal to the row ingest
/// would have written.
pub struct PriceFill<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub book: &'a PriceBook,
    /// Stamped into `metadata.priced_at` on every row this fill touches.
    pub priced_at: DateTime<Utc>,
    /// Rows per transaction. A fill can span a very large table; paging keeps the write lock (and,
    /// on Postgres, the transaction) bounded instead of holding ingest for the whole backfill.
    pub page: usize,
}

/// Rows per fill transaction when a caller states no preference.
pub const DEFAULT_FILL_PAGE: usize = 500;

impl<'a> PriceFill<'a> {
    pub fn new(provider: &'a str, model: &'a str, book: &'a PriceBook) -> Self {
        PriceFill {
            provider,
            model,
            book,
            priced_at: Utc::now(),
            page: DEFAULT_FILL_PAGE,
        }
    }

    /// The cost this fill assigns to one stored row, or `None` when the book still cannot price it
    /// (a fill for a *variant* key, say, that leaves the base model unpriced). `None` means leave
    /// the row alone: a fill must never turn an honest NULL into a wrong number.
    pub fn cost_for(&self, usage: &TokenUsage, mode: PricingMode) -> Option<f64> {
        self.book
            .cost_usd_mode(self.provider, self.model, usage, mode)
    }

    /// Apply this fill's provenance stamp to a row's `metadata` object.
    ///
    /// Overwrites nothing but the two fill keys — an event's metadata is the caller's, and a
    /// backfill that dropped a `customer_id` would silently rewrite who the spend belonged to.
    pub fn stamp(&self, metadata: &mut Value) {
        if !metadata.is_object() {
            *metadata = Value::Object(Map::new());
        }
        if let Some(m) = metadata.as_object_mut() {
            m.insert("cost_source".into(), Value::String(FILL_SOURCE.into()));
            m.insert(
                "priced_at".into(),
                Value::String(crate::codec::fmt_ts(self.priced_at)),
            );
        }
    }
}

/// The unpriced ledger, folded out of [`Store::rollup`].
///
/// Grouped by `provider × model × day` so the day buckets give first/last-seen without a second
/// query; keyed on `received_at`, matching every other accounting read (a caller that backdates its
/// events must not be able to hide its unpriced traffic from the ledger either).
pub fn list_unpriced_via_rollup<S: Store + ?Sized>(
    store: &S,
    project: Option<&str>,
    since: DateTime<Utc>,
) -> Result<Vec<UnpricedRow>> {
    let q = RollupQuery::new(
        &[Dimension::Provider, Dimension::Model, Dimension::Day],
        since,
    )
    .project(project)
    .time_key(TimeKey::ReceivedAt)
    .only_unpriced();
    let rows = store.rollup(&q)?;

    let mut agg: BTreeMap<(String, String), UnpricedRow> = BTreeMap::new();
    for r in rows {
        // An event with no provider/model is stored as the empty string, never NULL; a NULL key
        // would mean the column is genuinely absent, and folding it under `""` keeps the parts
        // summing to the whole rather than dropping the bucket.
        let provider = r.key(0).unwrap_or_default().to_string();
        let model = r.key(1).unwrap_or_default().to_string();
        let Some(day) = r.key(2).and_then(day_start) else {
            continue;
        };
        let e = agg
            .entry((provider.clone(), model.clone()))
            .or_insert_with(|| UnpricedRow {
                provider,
                model,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                first_seen: day,
                last_seen: day,
            });
        e.calls += r.calls;
        e.input_tokens += r.input_tokens;
        e.output_tokens += r.output_tokens;
        e.first_seen = e.first_seen.min(day);
        e.last_seen = e.last_seen.max(day);
    }
    Ok(agg.into_values().collect())
}

/// The `YYYY-MM-DD` bucket key a `Dimension::Day` rollup returns, as midnight UTC.
fn day_start(day: &str) -> Option<DateTime<Utc>> {
    lighttrack_core::parse_price_date(day)
}

/// The current row per key out of a full (dated, append-only) price table: the latest
/// `effective_from <= at`.
///
/// The SQL backends narrow this in the query; this is the shared fold for a backend that can only
/// hand back every row (Firestore), so "current" means the same thing on all three.
pub fn current_rows(rows: Vec<ModelPriceRow>, at: DateTime<Utc>) -> Vec<ModelPriceRow> {
    let mut best: BTreeMap<(String, String), ModelPriceRow> = BTreeMap::new();
    for r in rows.into_iter().filter(|r| r.effective_from <= at) {
        match best.entry((r.provider.clone(), r.model.clone())) {
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if r.effective_from >= o.get().effective_from {
                    o.insert(r);
                }
            }
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(r);
            }
        }
    }
    best.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, input: f64, from: &str) -> ModelPriceRow {
        ModelPriceRow {
            provider: "openai".into(),
            model: model.into(),
            input_per_mtok: input,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
            effective_from: lighttrack_core::parse_price_date(from).expect("date"),
            source_url: None,
            verified_at: None,
            note: None,
        }
    }

    #[test]
    fn current_rows_keeps_one_row_per_key_and_hides_the_future() {
        let at = lighttrack_core::parse_price_date("2026-06-15").expect("date");
        let out = current_rows(
            vec![
                row("a", 1.0, "2026-01-01"),
                row("a", 2.0, "2026-06-01"),
                row("a", 9.0, "2027-01-01"),
                row("b", 5.0, "2026-02-01"),
            ],
            at,
        );
        assert_eq!(out.len(), 2);
        let a = out.iter().find(|r| r.model == "a").expect("a");
        assert_eq!(
            a.input_per_mtok, 2.0,
            "the June correction, not the 2027 row"
        );
    }

    #[test]
    fn the_stamp_labels_a_fill_without_touching_the_callers_metadata() {
        let book = PriceBook::default();
        let f = PriceFill::new("openai", "gpt-9", &book);
        let mut meta = serde_json::json!({"customer_id": "acme"});
        f.stamp(&mut meta);
        assert_eq!(meta["customer_id"], "acme", "caller metadata survives");
        assert_eq!(meta["cost_source"], FILL_SOURCE);
        assert!(meta["priced_at"].is_string());

        // A row with no metadata at all still gets a well-formed object.
        let mut none = Value::Null;
        f.stamp(&mut none);
        assert_eq!(none["cost_source"], FILL_SOURCE);
    }
}
