//! Rubrics (structured, weighted judging criteria).

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::Rubric;
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "id, project_id, name, dimensions, threshold, created_at";

pub(crate) async fn create(pool: &PgPool, r: &Rubric) -> Result<()> {
    let dims = serde_json::to_string(&r.dimensions)?;
    sqlx::query(
        "INSERT INTO rubrics (id, project_id, name, dimensions, threshold, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(r.id.clone())
    .bind(r.project_id.clone())
    .bind(r.name.clone())
    .bind(dims)
    .bind(r.threshold)
    .bind(fmt_ts(r.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Rubric>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM rubrics WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool, project: &str) -> Result<Vec<Rubric>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM rubrics WHERE project_id = $1 ORDER BY created_at DESC"
    ))
    .bind(project.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<Rubric> {
    let dims: String = row.try_get(3).map_err(pgerr)?;
    let created_at: String = row.try_get(5).map_err(pgerr)?;
    Ok(Rubric {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        name: row.try_get(2).map_err(pgerr)?,
        dimensions: serde_json::from_str(&dims)?,
        threshold: row.try_get(4).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}
