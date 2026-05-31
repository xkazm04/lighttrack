//! Scores: persist judge verdicts; read them back.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::Score;
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "id, project_id, event_id, rubric, value, \"max\", pass, reasoning, \
    scored_by, cost_usd, created_at";

pub(crate) async fn insert(pool: &PgPool, s: &Score) -> Result<()> {
    sqlx::query(
        "INSERT INTO scores (id, project_id, event_id, rubric, value, \"max\", pass, \
         reasoning, scored_by, cost_usd, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    )
    .bind(s.id.clone())
    .bind(s.project_id.clone())
    .bind(s.event_id.clone())
    .bind(s.rubric.clone())
    .bind(s.value)
    .bind(s.max)
    .bind(s.pass.map(|b| b as i64))
    .bind(s.reasoning.clone())
    .bind(s.scored_by.clone())
    .bind(s.cost_usd)
    .bind(fmt_ts(s.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list(pool: &PgPool, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
    let rows = match project {
        Some(p) => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM scores WHERE project_id = $1 ORDER BY created_at DESC LIMIT $2"
            ))
            .bind(p.to_string())
            .bind(limit as i64)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(&format!("SELECT {COLS} FROM scores ORDER BY created_at DESC LIMIT $1"))
                .bind(limit as i64)
                .fetch_all(pool)
                .await
        }
    }
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn from_row(row: &PgRow) -> Result<Score> {
    let created_at: String = row.try_get(10).map_err(pgerr)?;
    Ok(Score {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        event_id: row.try_get(2).map_err(pgerr)?,
        rubric: row.try_get(3).map_err(pgerr)?,
        value: row.try_get(4).map_err(pgerr)?,
        max: row.try_get(5).map_err(pgerr)?,
        pass: row.try_get::<Option<i64>, _>(6).map_err(pgerr)?.map(|v| v != 0),
        reasoning: row.try_get(7).map_err(pgerr)?,
        scored_by: row.try_get(8).map_err(pgerr)?,
        cost_usd: row.try_get(9).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}
