//! DB-backed model price book — **dated and append-only** since M26.
//!
//! A row is keyed `(provider, model, effective_from)`, so writing a corrected rate adds a row and
//! leaves the one that priced last quarter's traffic intact. [`list`] answers "what are we charging
//! now" (the latest row that has taken effect, per key); [`history`] answers "what were we charging
//! in June", which is the only way a June cost number can be defended.

use chrono::Utc;
use rusqlite::{params, Connection, Row};

use lighttrack_core::ModelPriceRow;

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "provider, model, input_per_mtok, output_per_mtok, \
    cached_input_per_mtok, effective_from, source_url, verified_at, note";

/// Store one dated rate. Re-writing the *same* `(provider, model, effective_from)` updates it —
/// that is a correction to one point on the timeline, not a rewrite of the timeline.
pub(super) fn upsert(conn: &Connection, p: &ModelPriceRow) -> Result<()> {
    conn.execute(
        "INSERT INTO model_prices \
         (provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok, \
          effective_from, source_url, verified_at, note) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
         ON CONFLICT(provider, model, effective_from) DO UPDATE SET \
           input_per_mtok=excluded.input_per_mtok, output_per_mtok=excluded.output_per_mtok, \
           cached_input_per_mtok=excluded.cached_input_per_mtok, \
           source_url=excluded.source_url, verified_at=excluded.verified_at, note=excluded.note",
        params![
            p.provider,
            p.model,
            p.input_per_mtok,
            p.output_per_mtok,
            p.cached_input_per_mtok,
            fmt_ts(p.effective_from),
            p.source_url,
            p.verified_at.map(fmt_ts),
            p.note,
        ],
    )?;
    Ok(())
}

/// The **current** book: one row per key, the latest `effective_from` that is not in the future.
///
/// A future-dated row (a rate announced ahead of its switch-over) is stored and returned by
/// [`history`], but must not price today's traffic — hence the `<= now` bound here as well as in
/// `PriceBook::from_rows_at`.
pub(super) fn list(conn: &Connection) -> Result<Vec<ModelPriceRow>> {
    let sql = format!(
        "SELECT {COLS} FROM model_prices p WHERE p.effective_from <= ?1 \
           AND p.effective_from = ( \
             SELECT MAX(q.effective_from) FROM model_prices q \
              WHERE q.provider = p.provider AND q.model = p.model AND q.effective_from <= ?1) \
         ORDER BY provider, model"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map([fmt_ts(Utc::now())], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Every stored rate for one key, newest first — the price timeline.
pub(super) fn history(
    conn: &Connection,
    provider: &str,
    model: &str,
) -> Result<Vec<ModelPriceRow>> {
    let sql = format!(
        "SELECT {COLS} FROM model_prices WHERE provider = ?1 AND model = ?2 \
         ORDER BY effective_from DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![provider, model], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

type PriceRaw = (
    String,
    String,
    f64,
    f64,
    Option<f64>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn map_raw(row: &Row) -> rusqlite::Result<PriceRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn from_raw(r: PriceRaw) -> Result<ModelPriceRow> {
    Ok(ModelPriceRow {
        provider: r.0,
        model: r.1,
        input_per_mtok: r.2,
        output_per_mtok: r.3,
        cached_input_per_mtok: r.4,
        effective_from: parse_ts(&r.5)?,
        source_url: r.6,
        verified_at: r.7.as_deref().map(parse_ts).transpose()?,
        note: r.8,
    })
}

#[cfg(test)]
mod cols_tests {
    use super::*;

    /// `events` and `scores` derive their list from the schema model; this one is hand-kept and
    /// read by position, so it is asserted against the model instead — it fails the moment a
    /// column is added to one and not the other.
    #[test]
    fn cols_match_the_schema_model() {
        use crate::schema::{tables, Dialect};
        assert_eq!(COLS, tables::MODEL_PRICES.select_list(Dialect::Sqlite));
    }
}
