//! Rubrics (weighted, anchored dimensions).

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::Rubric;

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "id, project_id, name, dimensions, threshold, created_at, version, supersedes";

pub(super) fn create(conn: &Connection, r: &Rubric) -> Result<()> {
    let dims = serde_json::to_string(&r.dimensions)?;
    conn.execute(
        "INSERT INTO rubrics \
         (id, project_id, name, dimensions, threshold, created_at, version, supersedes) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            r.id,
            r.project_id,
            r.name,
            dims,
            r.threshold,
            fmt_ts(r.created_at),
            r.version as i64,
            r.supersedes,
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, project: Option<&str>, id: &str) -> Result<Option<Rubric>> {
    let sql = format!(
        "SELECT {COLS} FROM rubrics WHERE id = ?1{}",
        super::scope_and(2)
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id, project], map_raw).optional()?;
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

type RubricRaw = (
    String,
    String,
    String,
    String,
    f64,
    String,
    Option<i64>,
    Option<String>,
);

fn map_raw(row: &Row) -> rusqlite::Result<RubricRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
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
        // A row written before versioning is generation 1 — the same reading `Rubric`'s serde
        // default takes, so a stored rubric and a posted one agree.
        version: r.6.unwrap_or(1).max(1) as u32,
        supersedes: r.7,
    })
}
