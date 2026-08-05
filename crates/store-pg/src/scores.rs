//! Scores: persist judge verdicts; read them back.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Score, ScoreDetail};
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

pub(crate) const COLS: &str =
    "id, project_id, event_id, rubric, value, \"max\", pass, reasoning, detail, \
    run_id, case_index, scored_by, cost_usd, created_at";

pub(crate) async fn insert(pool: &PgPool, s: &Score) -> Result<()> {
    // Verdict provenance rides as JSON in one column (as on SQLite): it is read back whole with the
    // score and never filtered on, so a per-dimension table would buy nothing but joins.
    let detail = match &s.detail {
        Some(d) if !d.is_empty() => {
            Some(serde_json::to_string(d).map_err(lighttrack_store::StoreError::from)?)
        }
        _ => None,
    };
    sqlx::query(
        "INSERT INTO scores (id, project_id, event_id, rubric, value, \"max\", pass, \
         reasoning, detail, run_id, case_index, scored_by, cost_usd, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
    )
    .bind(s.id.clone())
    .bind(s.project_id.clone())
    .bind(s.event_id.clone())
    .bind(s.rubric.clone())
    .bind(s.value)
    .bind(s.max)
    .bind(s.pass.map(|b| b as i64))
    .bind(s.reasoning.clone())
    .bind(detail)
    .bind(s.run_id.clone())
    .bind(s.case_index.map(|i| i as i64))
    .bind(s.scored_by.clone())
    .bind(s.cost_usd)
    .bind(fmt_ts(s.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

/// Every case result recorded for one benchmark run, in case order. Rides `idx_scores_run`.
/// `project` is the caller's authorization scope (`None` = admin), applied in the query so a
/// project key can never read another project's run.
pub(crate) async fn list_by_run(
    pool: &PgPool,
    run_id: &str,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<Score>> {
    // `case_index IS NULL` leads the ORDER BY so NULL placement matches SQLite (which sorts NULLs
    // first) instead of Postgres' default NULLS LAST — one order, asserted once in conformance.
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM scores WHERE run_id = $1 AND ($2::text IS NULL OR project_id = $2) \
         ORDER BY (case_index IS NULL), case_index, created_at LIMIT $3"
    ))
    .bind(run_id.to_string())
    .bind(project.map(str::to_string))
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
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
            sqlx::query(&format!(
                "SELECT {COLS} FROM scores ORDER BY created_at DESC LIMIT $1"
            ))
            .bind(limit as i64)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// The subset of `event_ids` that already carry at least one score. `= ANY($1)` over the id array
/// rides `idx_scores_event`; scoped to the given ids so it never scans the whole scores table.
pub(crate) async fn scored_event_ids(pool: &PgPool, event_ids: &[String]) -> Result<Vec<String>> {
    if event_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT DISTINCT event_id FROM scores WHERE event_id = ANY($1) AND event_id IS NOT NULL",
    )
    .bind(event_ids)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter()
        .map(|r| r.try_get::<String, _>(0).map_err(pgerr))
        .collect()
}

pub(crate) fn from_row(row: &PgRow) -> Result<Score> {
    let created_at: String = row.try_get(13).map_err(pgerr)?;
    // A detail blob written by a newer/other writer must not sink the whole listing: an unreadable
    // one degrades to `None` (the score's scalar is still true) rather than erroring the query.
    let detail = row
        .try_get::<Option<String>, _>(8)
        .map_err(pgerr)?
        .as_deref()
        .and_then(|j| serde_json::from_str::<ScoreDetail>(j).ok());
    Ok(Score {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        event_id: row.try_get(2).map_err(pgerr)?,
        rubric: row.try_get(3).map_err(pgerr)?,
        value: row.try_get(4).map_err(pgerr)?,
        max: row.try_get(5).map_err(pgerr)?,
        pass: row
            .try_get::<Option<i64>, _>(6)
            .map_err(pgerr)?
            .map(|v| v != 0),
        reasoning: row.try_get(7).map_err(pgerr)?,
        detail,
        run_id: row.try_get(9).map_err(pgerr)?,
        case_index: row
            .try_get::<Option<i64>, _>(10)
            .map_err(pgerr)?
            .map(|i| i as u32),
        scored_by: row.try_get(11).map_err(pgerr)?,
        cost_usd: row.try_get(12).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    /// `from_row` reads by position; adding a column mid-list without moving the reads shifts every
    /// field after it, and most are strings, so nothing would fail to compile.
    #[test]
    fn cols_match_the_positions_from_row_reads() {
        let names = select_list_names(COLS);
        assert_eq!(
            names,
            [
                "id",
                "project_id",
                "event_id",
                "rubric",
                "value",
                "\"max\"",
                "pass",
                "reasoning",
                "detail",
                "run_id",
                "case_index",
                "scored_by",
                "cost_usd",
                "created_at"
            ]
        );
    }

    /// `traces::list_scores_by_trace` aliases these columns by splitting on ", " and prefixing each
    /// with `s.`. That is only correct while every entry is a bare identifier: a `COALESCE(a, b)` or
    /// an `AS` alias here would be split mid-expression and produce invalid SQL for the join read.
    #[test]
    fn cols_stay_bare_identifiers_for_the_trace_join() {
        for c in COLS.split(", ") {
            let c = c.trim();
            assert!(
                !c.contains('(') && !c.contains(" AS ") && !c.contains(' '),
                "score column {c:?} is not a bare identifier"
            );
        }
    }
}
