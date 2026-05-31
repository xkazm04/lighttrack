//! Model price book (base + tier/batch variant rows are just ordinary rows here).

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::ModelPriceRow;
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "provider, model, input_per_mtok, output_per_mtok, \
    cached_input_per_mtok, effective_date, source_url";

pub(crate) async fn upsert(pool: &PgPool, p: &ModelPriceRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO model_prices (provider, model, input_per_mtok, output_per_mtok, \
         cached_input_per_mtok, effective_date, source_url) VALUES ($1,$2,$3,$4,$5,$6,$7) \
         ON CONFLICT (provider, model) DO UPDATE SET \
           input_per_mtok = EXCLUDED.input_per_mtok, output_per_mtok = EXCLUDED.output_per_mtok, \
           cached_input_per_mtok = EXCLUDED.cached_input_per_mtok, \
           effective_date = EXCLUDED.effective_date, source_url = EXCLUDED.source_url",
    )
    .bind(p.provider.clone())
    .bind(p.model.clone())
    .bind(p.input_per_mtok)
    .bind(p.output_per_mtok)
    .bind(p.cached_input_per_mtok)
    .bind(fmt_ts(p.effective_date))
    .bind(p.source_url.clone())
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list(pool: &PgPool) -> Result<Vec<ModelPriceRow>> {
    let rows = sqlx::query(&format!("SELECT {COLS} FROM model_prices ORDER BY provider, model"))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<ModelPriceRow> {
    let effective_date: String = row.try_get(5).map_err(pgerr)?;
    Ok(ModelPriceRow {
        provider: row.try_get(0).map_err(pgerr)?,
        model: row.try_get(1).map_err(pgerr)?,
        input_per_mtok: row.try_get(2).map_err(pgerr)?,
        output_per_mtok: row.try_get(3).map_err(pgerr)?,
        cached_input_per_mtok: row.try_get(4).map_err(pgerr)?,
        effective_date: parse_ts(&effective_date)?,
        source_url: row.try_get(6).map_err(pgerr)?,
    })
}
