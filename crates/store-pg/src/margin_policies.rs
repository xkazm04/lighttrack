//! Margin guardrail policies over the `margin_policies` table.
//!
//! Mirrors `lighttrack-store`'s SQLite module method for method: `trigger`/`action` are stored as
//! JSON text because both are open sum types, and this is sweep-time configuration, never hot-path
//! data. Parity here is not decoration — a Postgres deployment that answered `[]` instead of
//! `Unsupported` would show an operator an empty guardrail list and let them conclude the feature
//! was off rather than unported.

use sqlx::{PgPool, Row};

use lighttrack_core::{MarginPolicy, PolicyAction, PolicyTrigger};
use lighttrack_store::{Result, StoreError};

use crate::util::pgerr;

const COLS: &str =
    "id, project_id, trigger_json, min_cost_usd, action_json, cooldown_secs, expiry_secs, enabled";

pub(crate) async fn create(pool: &PgPool, p: &MarginPolicy) -> Result<()> {
    sqlx::query(
        "INSERT INTO margin_policies \
         (id, project_id, trigger_json, min_cost_usd, action_json, cooldown_secs, expiry_secs, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(p.id.clone())
    .bind(p.project_id.clone())
    .bind(serde_json::to_string(&p.trigger)?)
    .bind(p.min_cost_usd)
    .bind(serde_json::to_string(&p.action)?)
    .bind(p.cooldown_secs as i64)
    .bind(p.expiry_secs as i64)
    .bind(p.enabled as i64)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list(
    pool: &PgPool,
    project: &str,
    only_enabled: bool,
) -> Result<Vec<MarginPolicy>> {
    let sql = if only_enabled {
        format!("SELECT {COLS} FROM margin_policies WHERE project_id = $1 AND enabled = 1")
    } else {
        format!("SELECT {COLS} FROM margin_policies WHERE project_id = $1")
    };
    let rows = sqlx::query(&sql)
        .bind(project.to_string())
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<MarginPolicy>> {
    let sql = format!("SELECT {COLS} FROM margin_policies WHERE id = $1");
    let row = sqlx::query(&sql)
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn delete(pool: &PgPool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM margin_policies WHERE id = $1")
        .bind(id.to_string())
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(res.rows_affected() > 0)
}

fn from_row(row: &sqlx::postgres::PgRow) -> Result<MarginPolicy> {
    let id: String = row.try_get(0).map_err(pgerr)?;
    let trigger_json: String = row.try_get(2).map_err(pgerr)?;
    let action_json: String = row.try_get(4).map_err(pgerr)?;
    let trigger: PolicyTrigger = serde_json::from_str(&trigger_json).map_err(|e| {
        StoreError::Other(format!(
            "margin policy '{id}' has an unreadable trigger: {e}"
        ))
    })?;
    let action: PolicyAction = serde_json::from_str(&action_json).map_err(|e| {
        StoreError::Other(format!(
            "margin policy '{id}' has an unreadable action: {e}"
        ))
    })?;
    Ok(MarginPolicy {
        id,
        project_id: row.try_get(1).map_err(pgerr)?,
        trigger,
        min_cost_usd: row.try_get(3).map_err(pgerr)?,
        action,
        cooldown_secs: row.try_get::<i64, _>(5).map_err(pgerr)?.max(0) as u64,
        expiry_secs: row.try_get::<i64, _>(6).map_err(pgerr)?.max(0) as u64,
        enabled: row.try_get::<i64, _>(7).map_err(pgerr)? != 0,
    })
}
