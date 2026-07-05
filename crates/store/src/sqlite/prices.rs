//! DB-backed model price book.

use rusqlite::{params, Connection, Row};

use lighttrack_core::ModelPriceRow;

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "provider, model, input_per_mtok, output_per_mtok, \
    cached_input_per_mtok, effective_date, source_url";

pub(super) fn upsert(conn: &Connection, p: &ModelPriceRow) -> Result<()> {
    conn.execute(
        "INSERT INTO model_prices \
         (provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok, effective_date, source_url) \
         VALUES (?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(provider, model) DO UPDATE SET \
           input_per_mtok=excluded.input_per_mtok, output_per_mtok=excluded.output_per_mtok, \
           cached_input_per_mtok=excluded.cached_input_per_mtok, \
           effective_date=excluded.effective_date, source_url=excluded.source_url",
        params![
            p.provider,
            p.model,
            p.input_per_mtok,
            p.output_per_mtok,
            p.cached_input_per_mtok,
            fmt_ts(p.effective_date),
            p.source_url,
        ],
    )?;
    Ok(())
}

pub(super) fn list(conn: &Connection) -> Result<Vec<ModelPriceRow>> {
    let sql = format!("SELECT {COLS} FROM model_prices ORDER BY provider, model");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt.query_map([], map_raw)?.collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

type PriceRaw = (String, String, f64, f64, Option<f64>, String, Option<String>);

fn map_raw(row: &Row) -> rusqlite::Result<PriceRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn from_raw(r: PriceRaw) -> Result<ModelPriceRow> {
    Ok(ModelPriceRow {
        provider: r.0,
        model: r.1,
        input_per_mtok: r.2,
        output_per_mtok: r.3,
        cached_input_per_mtok: r.4,
        effective_date: parse_ts(&r.5)?,
        source_url: r.6,
    })
}
