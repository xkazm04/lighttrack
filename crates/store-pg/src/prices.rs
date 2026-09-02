//! Model price book — **dated and append-only** since M26.
//!
//! Keyed `(provider, model, effective_from)`: a corrected rate appends a row rather than
//! overwriting the one that priced last quarter's traffic. Tier and batch/flex variant rows are
//! still just ordinary rows here (the modifier lives in `model`).

use chrono::Utc;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::ModelPriceRow;
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "provider, model, input_per_mtok, output_per_mtok, \
    cached_input_per_mtok, effective_from, source_url, verified_at, note";

pub(crate) async fn upsert(pool: &PgPool, p: &ModelPriceRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_prices (provider, model, input_per_mtok, output_per_mtok, \
         cached_input_per_mtok, effective_from, source_url, verified_at, note) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
         ON CONFLICT (provider, model, effective_from) DO UPDATE SET \
           input_per_mtok = EXCLUDED.input_per_mtok, output_per_mtok = EXCLUDED.output_per_mtok, \
           cached_input_per_mtok = EXCLUDED.cached_input_per_mtok, \
           source_url = EXCLUDED.source_url, verified_at = EXCLUDED.verified_at, \
           note = EXCLUDED.note",
    )
    .bind(p.provider.clone())
    .bind(p.model.clone())
    .bind(p.input_per_mtok)
    .bind(p.output_per_mtok)
    .bind(p.cached_input_per_mtok)
    .bind(fmt_ts(p.effective_from))
    .bind(p.source_url.clone())
    .bind(p.verified_at.map(fmt_ts))
    .bind(p.note.clone())
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

/// The **current** book: one row per key, the latest `effective_from` not in the future. A
/// future-dated rate is stored and visible in [`history`], but must not price today's traffic.
pub(crate) async fn list(pool: &PgPool) -> Result<Vec<ModelPriceRow>> {
    let rows = sqlx::query(&format!(
        "SELECT DISTINCT ON (provider, model) {COLS} FROM model_prices \
          WHERE effective_from <= $1 \
          ORDER BY provider, model, effective_from DESC"
    ))
    .bind(fmt_ts(Utc::now()))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// Every stored rate for one key, newest first — the price timeline.
pub(crate) async fn history(
    pool: &PgPool,
    provider: &str,
    model: &str,
) -> Result<Vec<ModelPriceRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM model_prices WHERE provider = $1 AND model = $2 \
         ORDER BY effective_from DESC"
    ))
    .bind(provider.to_string())
    .bind(model.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<ModelPriceRow> {
    let effective_from: String = row.try_get(5).map_err(pgerr)?;
    let verified_at: Option<String> = row.try_get(7).map_err(pgerr)?;
    Ok(ModelPriceRow {
        provider: row.try_get(0).map_err(pgerr)?,
        model: row.try_get(1).map_err(pgerr)?,
        input_per_mtok: row.try_get(2).map_err(pgerr)?,
        output_per_mtok: row.try_get(3).map_err(pgerr)?,
        cached_input_per_mtok: row.try_get(4).map_err(pgerr)?,
        effective_from: parse_ts(&effective_from)?,
        source_url: row.try_get(6).map_err(pgerr)?,
        verified_at: verified_at.as_deref().map(parse_ts).transpose()?,
        note: row.try_get(8).map_err(pgerr)?,
    })
}
