//! Cost/usage rollups over `events`: by (project, provider, model), and by use-case name.

use sqlx::postgres::PgPool;
use sqlx::Row;

use lighttrack_store::{CostRow, Result, UseCaseCostRow};

use super::filters::window_conds;
use crate::util::pgerr;

pub(crate) async fn cost_summary(pool: &PgPool, project: Option<&str>) -> Result<Vec<CostRow>> {
    cost_summary_windowed(pool, project, None, None).await
}

/// Cost/usage rollup over an optional `[since, until)` window. Same grouping/ordering as
/// [`cost_summary`]; window bounds compare against the fixed-width `ts` string.
pub(crate) async fn cost_summary_windowed(
    pool: &PgPool,
    project: Option<&str>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<CostRow>> {
    let cols = "project_id, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0)::bigint AS it, COALESCE(SUM(output_tokens),0)::bigint AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost, \
        COUNT(*) FILTER (WHERE cost_usd IS NULL)::bigint AS unpriced";
    let conds = window_conds(project, since, until);
    let where_clause = conds.where_clause();
    let sql = format!(
        "SELECT {cols} FROM events {where_clause}\
         GROUP BY project_id, provider, model ORDER BY cost DESC"
    );
    let mut q = sqlx::query(&sql);
    for b in conds.binds() {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter()
        .map(|row| {
            Ok(CostRow {
                project_id: row.try_get(0).map_err(pgerr)?,
                provider: row.try_get(1).map_err(pgerr)?,
                model: row.try_get(2).map_err(pgerr)?,
                calls: row.try_get(3).map_err(pgerr)?,
                input_tokens: row.try_get(4).map_err(pgerr)?,
                output_tokens: row.try_get(5).map_err(pgerr)?,
                cost_usd: row.try_get(6).map_err(pgerr)?,
                unpriced_calls: row.try_get(7).map_err(pgerr)?,
            })
        })
        .collect()
}

/// Use-case rollup grouped by (name, provider, model), optionally windowed by `since`. Un-named
/// calls group together per model; ordered by cost, most expensive first.
pub(crate) async fn usecase_costs(
    pool: &PgPool,
    project: Option<&str>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<UseCaseCostRow>> {
    let cols = "name, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0)::bigint AS it, COALESCE(SUM(output_tokens),0)::bigint AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost, \
        COUNT(*) FILTER (WHERE cost_usd IS NULL)::bigint AS unpriced";
    let conds = window_conds(project, since, None);
    let where_clause = conds.where_clause();
    let sql = format!(
        "SELECT {cols} FROM events {where_clause}GROUP BY name, provider, model ORDER BY cost DESC"
    );
    let mut q = sqlx::query(&sql);
    for b in conds.binds() {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter()
        .map(|row| {
            Ok(UseCaseCostRow {
                name: row.try_get(0).map_err(pgerr)?,
                provider: row.try_get(1).map_err(pgerr)?,
                model: row.try_get(2).map_err(pgerr)?,
                calls: row.try_get(3).map_err(pgerr)?,
                input_tokens: row.try_get(4).map_err(pgerr)?,
                output_tokens: row.try_get(5).map_err(pgerr)?,
                cost_usd: row.try_get(6).map_err(pgerr)?,
                unpriced_calls: row.try_get(7).map_err(pgerr)?,
            })
        })
        .collect()
}
