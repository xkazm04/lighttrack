//! `Surface::Calibrations`: the stored judge↔human agreement history, and the trust lookup (M11).
//!
//! Append-only — a re-measurement is a new row, because the history is what a drift check reads.
//! [`latest`] is exact on the rubric, `NULL` included: a rubric never inherits the freeform
//! measurement or a sibling's, which is also what makes a new rubric version start `unknown`.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::CalibrationRecord;
use lighttrack_store::{Result, StoreError};

use lighttrack_store::codec::decode_event_cursor;

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "id, project_id, judge, rubric_id, dataset_id, dataset_version, kappa, \
     pearson, mae, rmse, n, kappa_bar, trusted, created_at";

pub(crate) async fn insert(pool: &PgPool, c: &CalibrationRecord) -> Result<()> {
    sqlx::query(&format!(
        "INSERT INTO calibrations ({COLS}) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)"
    ))
    .bind(c.id.clone())
    .bind(c.project_id.clone())
    .bind(c.judge.clone())
    .bind(c.rubric_id.clone())
    .bind(c.dataset_id.clone())
    .bind(c.dataset_version.map(|v| v as i32))
    .bind(c.kappa)
    .bind(c.pearson)
    .bind(c.mae)
    .bind(c.rmse)
    .bind(c.n as i32)
    .bind(c.kappa_bar)
    .bind(c.trusted)
    .bind(fmt_ts(c.created_at))
    .execute(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            StoreError::Conflict(format!("calibration '{}' already exists", c.id))
        }
        _ => pgerr(e),
    })?;
    Ok(())
}

/// `rubric_id IS NULL` is matched as `IS NULL`, not `= NULL` — the latter is never true in SQL, so
/// a freeform calibration would be unfindable and every freeform judge would read as `unknown`.
pub(crate) async fn latest(
    pool: &PgPool,
    project: &str,
    rubric_id: Option<&str>,
    judge: &str,
) -> Result<Option<CalibrationRecord>> {
    let clause = match rubric_id {
        Some(_) => "rubric_id = $3",
        None => "rubric_id IS NULL",
    };
    let sql = format!(
        "SELECT {COLS} FROM calibrations \
         WHERE project_id = $1 AND judge = $2 AND {clause} \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    );
    let mut q = sqlx::query(&sql)
        .bind(project.to_string())
        .bind(judge.to_string());
    if let Some(r) = rubric_id {
        q = q.bind(r.to_string());
    }
    let row = q.fetch_optional(pool).await.map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

/// Newest-first, keyset-paged on `(created_at, id)`.
pub(crate) async fn list(
    pool: &PgPool,
    project: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Vec<CalibrationRecord>> {
    let mut sql = format!("SELECT {COLS} FROM calibrations WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        binds.push(p.to_string());
        sql.push_str(&format!(" AND project_id = ${}", binds.len()));
    }
    if let Some((ts, id)) = cursor.and_then(decode_event_cursor) {
        binds.push(ts.clone());
        let a = binds.len();
        binds.push(ts);
        binds.push(id);
        sql.push_str(&format!(
            " AND (created_at < ${a} OR (created_at = ${} AND id < ${}))",
            a + 1,
            a + 2
        ));
    }
    let effective = match limit {
        0 => 100,
        n => n.min(1000),
    };
    sql.push_str(&format!(
        " ORDER BY created_at DESC, id DESC LIMIT {effective}"
    ));
    let mut q = sqlx::query(&sql);
    for b in binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<CalibrationRecord> {
    let created_at: String = row.try_get(13).map_err(pgerr)?;
    Ok(CalibrationRecord {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        judge: row.try_get(2).map_err(pgerr)?,
        rubric_id: row.try_get(3).map_err(pgerr)?,
        dataset_id: row.try_get(4).map_err(pgerr)?,
        dataset_version: row
            .try_get::<Option<i32>, _>(5)
            .map_err(pgerr)?
            .map(|v| v.max(0) as u32),
        kappa: row.try_get(6).map_err(pgerr)?,
        pearson: row.try_get(7).map_err(pgerr)?,
        mae: row.try_get(8).map_err(pgerr)?,
        rmse: row.try_get(9).map_err(pgerr)?,
        n: row.try_get::<i32, _>(10).map_err(pgerr)?.max(0) as u32,
        kappa_bar: row.try_get(11).map_err(pgerr)?,
        trusted: row.try_get(12).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    #[test]
    fn the_select_list_matches_the_positional_reads() {
        let names = select_list_names(COLS);
        assert_eq!(names.len(), 14);
        assert_eq!(names[0], "id");
        assert_eq!(names[10], "n");
        assert_eq!(names[12], "trusted");
        assert_eq!(names[13], "created_at");
    }
}
