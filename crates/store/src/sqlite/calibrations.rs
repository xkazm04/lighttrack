//! `Surface::Calibrations`: the stored judge↔human agreement history, and the trust lookup.
//!
//! Append-only. A re-measurement is a new row, never an update of the last one, because the history
//! *is* the product here: the drift check that used to scan 500 scores for a reserved rubric name is
//! now `latest_calibration` plus the row before it.
//!
//! [`latest`] is deliberately exact on the rubric, including on `NULL`: a rubric's trust is never
//! inherited from a freeform measurement or from a sibling rubric. That one rule is what makes a new
//! rubric version (M9 mints a new id) start at `unknown` instead of silently wearing its
//! predecessor's badge.

use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row};

use lighttrack_core::CalibrationRecord;

use crate::codec::{decode_event_cursor, fmt_ts, parse_ts};
use crate::{Result, StoreError};

const COLS: &str = "id, project_id, judge, rubric_id, dataset_id, dataset_version, kappa, \
     pearson, mae, rmse, n, kappa_bar, trusted, created_at";

pub(super) fn insert(conn: &Connection, c: &CalibrationRecord) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO calibrations ({COLS}) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"
        ),
        params![
            c.id,
            c.project_id,
            c.judge,
            c.rubric_id,
            c.dataset_id,
            c.dataset_version.map(|v| v as i64),
            c.kappa,
            c.pearson,
            c.mae,
            c.rmse,
            c.n as i64,
            c.kappa_bar,
            c.trusted as i64,
            fmt_ts(c.created_at),
        ],
    )
    .map_err(|e| match &e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation => {
            StoreError::Conflict(format!("calibration '{}' already exists", c.id))
        }
        _ => e.into(),
    })?;
    Ok(())
}

/// The newest record for exactly this `(project, rubric_id, judge)`.
///
/// `rubric_id IS NULL` is matched as `IS NULL` rather than `= NULL` — the latter is never true in
/// SQL, so a freeform calibration would have been unfindable and every freeform judge would have
/// read as `unknown` forever.
pub(super) fn latest(
    conn: &Connection,
    project: &str,
    rubric_id: Option<&str>,
    judge: &str,
) -> Result<Option<CalibrationRecord>> {
    let rubric_clause = match rubric_id {
        Some(_) => "rubric_id = ?3",
        None => "rubric_id IS NULL",
    };
    let sql = format!(
        "SELECT {COLS} FROM calibrations \
         WHERE project_id = ?1 AND judge = ?2 AND {rubric_clause} \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = match rubric_id {
        Some(r) => stmt.query_row(params![project, judge, r], map_raw),
        None => stmt.query_row(params![project, judge], map_raw),
    }
    .optional()?;
    raw.map(from_raw).transpose()
}

/// Newest-first, keyset-paged on `(created_at, id)`.
pub(super) fn list(
    conn: &Connection,
    project: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Vec<CalibrationRecord>> {
    let mut sql = format!("SELECT {COLS} FROM calibrations WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = project {
        args.push(Box::new(p.to_string()));
        sql.push_str(&format!(" AND project_id = ?{}", args.len()));
    }
    if let Some((ts, id)) = cursor.and_then(decode_event_cursor) {
        args.push(Box::new(ts.clone()));
        let a = args.len();
        args.push(Box::new(ts));
        args.push(Box::new(id));
        sql.push_str(&format!(
            " AND (created_at < ?{a} OR (created_at = ?{} AND id < ?{}))",
            a + 1,
            a + 2
        ));
    }
    sql.push_str(&format!(
        " ORDER BY created_at DESC, id DESC LIMIT {}",
        effective_limit(limit)
    ));
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let raws = stmt
        .query_map(refs.as_slice(), map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// `0` means the default; anything larger than the cap is clamped, so a caller cannot ask for the
/// whole table.
pub(super) fn effective_limit(limit: usize) -> usize {
    match limit {
        0 => 100,
        n => n.min(1000),
    }
}

type CalRaw = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    f64,
    f64,
    f64,
    f64,
    i64,
    f64,
    i64,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<CalRaw> {
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
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn from_raw(r: CalRaw) -> Result<CalibrationRecord> {
    Ok(CalibrationRecord {
        id: r.0,
        project_id: r.1,
        judge: r.2,
        rubric_id: r.3,
        dataset_id: r.4,
        dataset_version: r.5.map(|v| v.max(0) as u32),
        kappa: r.6,
        pearson: r.7,
        mae: r.8,
        rmse: r.9,
        n: r.10.max(0) as u32,
        kappa_bar: r.11,
        trusted: r.12 != 0,
        created_at: parse_ts(&r.13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_size_defaults_and_clamps() {
        assert_eq!(effective_limit(0), 100);
        assert_eq!(effective_limit(7), 7);
        assert_eq!(effective_limit(usize::MAX), 1000);
    }
}
