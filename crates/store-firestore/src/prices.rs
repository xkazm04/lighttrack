//! `model_prices` collection — **dated and append-only** since M26.
//!
//! Doc id is `<provider>__<model>__<effective_from>`, so a corrected rate is a new document beside
//! the one that priced last quarter's traffic instead of a replacement for it. Documents written
//! before M26 carry the two-part id and an `effective_date` field; [`price_from`] reads either
//! spelling, so an existing deployment keeps its book without a data migration.
//!
//! Firestore has no `GROUP BY`, so "the current book" is the same client-side fold the rest of this
//! backend uses — and it is the *shared* one (`lighttrack_store::pricing::current_rows`), so
//! "current" means exactly what it means on the SQL backends.

use chrono::Utc;
use serde_json::json;

use lighttrack_core::ModelPriceRow;
use lighttrack_store::pricing::current_rows;
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "model_prices";

/// The document id for one dated rate. `effective_from` is part of it — that is what makes the
/// collection append-only rather than one overwritten doc per model.
fn doc_id(p: &ModelPriceRow) -> String {
    format!("{}__{}__{}", p.provider, p.model, fmt_ts(p.effective_from))
}

pub(crate) fn upsert_price(rest: &Rest, p: &ModelPriceRow) -> Result<()> {
    let mut m = Fields::new();
    m.insert("provider".into(), json!(p.provider));
    m.insert("model".into(), json!(p.model));
    m.insert("input_per_mtok".into(), json!(p.input_per_mtok));
    m.insert("output_per_mtok".into(), json!(p.output_per_mtok));
    m.insert(
        "cached_input_per_mtok".into(),
        json!(p.cached_input_per_mtok),
    );
    m.insert("effective_from".into(), json!(fmt_ts(p.effective_from)));
    m.insert("source_url".into(), json!(p.source_url));
    m.insert("verified_at".into(), json!(p.verified_at.map(fmt_ts)));
    m.insert("note".into(), json!(p.note));
    rest.put_doc(COLL, &doc_id(p), &m)
}

/// The **current** book: one row per key, the latest rate that has taken effect.
pub(crate) fn list_prices(rest: &Rest) -> Result<Vec<ModelPriceRow>> {
    let mut rows = current_rows(all(rest)?, Utc::now());
    rows.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
    Ok(rows)
}

/// Every stored rate for one key, newest first — the price timeline.
pub(crate) fn history(rest: &Rest, provider: &str, model: &str) -> Result<Vec<ModelPriceRow>> {
    let mut rows: Vec<ModelPriceRow> = all(rest)?
        .into_iter()
        .filter(|p| p.provider == provider && p.model == model)
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.effective_from));
    Ok(rows)
}

fn all(rest: &Rest) -> Result<Vec<ModelPriceRow>> {
    let docs = rest.query(COLL, &[], None, None)?;
    docs.iter().map(price_from).collect()
}

fn price_from(m: &Fields) -> Result<ModelPriceRow> {
    // Either date spelling: a pre-M26 document carries `effective_date`, and rewriting every one of
    // them on upgrade would be a migration this backend has no transaction to run it in.
    let effective = match fstr(m, "effective_from") {
        Some(s) => s,
        None => freq(m, "effective_date")?,
    };
    Ok(ModelPriceRow {
        provider: freq(m, "provider")?,
        model: freq(m, "model")?,
        input_per_mtok: ff64(m, "input_per_mtok").unwrap_or(0.0),
        output_per_mtok: ff64(m, "output_per_mtok").unwrap_or(0.0),
        cached_input_per_mtok: ff64(m, "cached_input_per_mtok"),
        effective_from: parse_ts(&effective)?,
        source_url: fstr(m, "source_url"),
        verified_at: fstr(m, "verified_at").map(|s| parse_ts(&s)).transpose()?,
        note: fstr(m, "note"),
    })
}
