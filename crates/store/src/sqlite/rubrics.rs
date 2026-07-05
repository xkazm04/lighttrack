//! Rubrics (weighted, anchored dimensions).

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::Rubric;

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "id, project_id, name, dimensions, threshold, created_at";

pub(super) fn create(conn: &Connection, r: &Rubric) -> Result<()> {
    let dims = serde_json::to_string(&r.dimensions)?;
    conn.execute(
        "INSERT INTO rubrics (id, project_id, name, dimensions, threshold, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![r.id, r.project_id, r.name, dims, r.threshold, fmt_ts(r.created_at)],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Rubric>> {
    let sql = format!("SELECT {COLS} FROM rubrics WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn list(conn: &Connection, project: &str) -> Result<Vec<Rubric>> {
    let sql = format!("SELECT {COLS} FROM rubrics WHERE project_id = ?1 ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

type RubricRaw = (String, String, String, String, f64, String);

fn map_raw(row: &Row) -> rusqlite::Result<RubricRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn from_raw(r: RubricRaw) -> Result<Rubric> {
    Ok(Rubric {
        id: r.0,
        project_id: r.1,
        name: r.2,
        dimensions: serde_json::from_str(&r.3)?,
        threshold: r.4,
        created_at: parse_ts(&r.5)?,
    })
}
