//! Projects, API keys, and limit rules.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{ApiKey, LimitRule, LimitScope, Project, Redaction};
use lighttrack_store::Result;

use crate::util::{enum_to_str, fmt_ts, parse_enum, parse_ts, pgerr};

// --- projects ---------------------------------------------------------------

pub(crate) async fn create(pool: &PgPool, p: &Project) -> Result<()> {
    sqlx::query(
        "INSERT INTO projects (id, name, enabled, redaction, collective_opt_in, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(p.id.clone())
    .bind(p.name.clone())
    .bind(p.enabled as i64)
    .bind(enum_to_str(&p.redaction)?)
    .bind(p.collective_opt_in as i64)
    .bind(fmt_ts(p.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Project>> {
    let row = sqlx::query(
        "SELECT id, name, enabled, redaction, collective_opt_in, created_at \
         FROM projects WHERE id = $1",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(project_from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool) -> Result<Vec<Project>> {
    let rows = sqlx::query(
        "SELECT id, name, enabled, redaction, collective_opt_in, created_at \
         FROM projects ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(project_from_row).collect()
}

// --- API keys ---------------------------------------------------------------

pub(crate) async fn create_key(pool: &PgPool, k: &ApiKey) -> Result<()> {
    sqlx::query(
        "INSERT INTO api_keys (id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(k.id.clone())
    .bind(k.project_id.clone())
    .bind(k.name.clone())
    .bind(k.prefix.clone())
    .bind(k.key_hash.clone())
    .bind(fmt_ts(k.created_at))
    .bind(k.last_used_at.map(fmt_ts))
    .bind(k.revoked as i64)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn find_key_by_prefix(pool: &PgPool, prefix: &str) -> Result<Option<ApiKey>> {
    let row = sqlx::query(
        "SELECT id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked \
         FROM api_keys WHERE prefix = $1",
    )
    .bind(prefix.to_string())
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(api_key_from_row).transpose()
}

pub(crate) async fn touch_key(pool: &PgPool, id: &str, when: DateTime<Utc>) -> Result<()> {
    sqlx::query("UPDATE api_keys SET last_used_at = $2 WHERE id = $1")
        .bind(id.to_string())
        .bind(fmt_ts(when))
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list_keys(pool: &PgPool, project: &str) -> Result<Vec<ApiKey>> {
    let rows = sqlx::query(
        "SELECT id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked \
         FROM api_keys WHERE project_id = $1 ORDER BY created_at DESC",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(api_key_from_row).collect()
}

pub(crate) async fn set_key_revoked(pool: &PgPool, id: &str, revoked: bool) -> Result<bool> {
    let res = sqlx::query("UPDATE api_keys SET revoked = $2 WHERE id = $1")
        .bind(id.to_string())
        .bind(revoked as i64)
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(res.rows_affected() > 0)
}

// --- limit rules ------------------------------------------------------------

/// The columns a rule row exposes, in the order [`limit_rule_from_row`] reads them.
const LIMIT_COLS: &str =
    "id, project_id, metric, \"window\", threshold, action, enabled, warn_at, scope_kind, scope_value";

/// Split an optional scope into its `(kind, value)` column pair (both `None` when unscoped).
fn scope_parts(scope: &Option<LimitScope>) -> (Option<&'static str>, Option<String>) {
    match scope {
        None => (None, None),
        Some(s) => (Some(s.kind_str()), Some(s.value().to_string())),
    }
}

pub(crate) async fn create_limit(pool: &PgPool, r: &LimitRule) -> Result<()> {
    let (scope_kind, scope_value) = scope_parts(&r.scope);
    sqlx::query(
        "INSERT INTO limit_rules \
         (id, project_id, metric, \"window\", threshold, action, enabled, warn_at, scope_kind, scope_value) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(r.id.clone())
    .bind(r.project_id.clone())
    .bind(enum_to_str(&r.metric)?)
    .bind(enum_to_str(&r.window)?)
    .bind(r.threshold)
    .bind(enum_to_str(&r.action)?)
    .bind(r.enabled as i64)
    .bind(r.warn_at)
    .bind(scope_kind)
    .bind(scope_value)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list_limits(pool: &PgPool, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
    let sql = if only_enabled {
        format!("SELECT {LIMIT_COLS} FROM limit_rules WHERE project_id = $1 AND enabled = 1")
    } else {
        format!("SELECT {LIMIT_COLS} FROM limit_rules WHERE project_id = $1")
    };
    let rows = sqlx::query(&sql).bind(project.to_string()).fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().map(limit_rule_from_row).collect()
}

pub(crate) async fn get_limit(pool: &PgPool, id: &str) -> Result<Option<LimitRule>> {
    let sql = format!("SELECT {LIMIT_COLS} FROM limit_rules WHERE id = $1");
    let row = sqlx::query(&sql).bind(id.to_string()).fetch_optional(pool).await.map_err(pgerr)?;
    row.as_ref().map(limit_rule_from_row).transpose()
}

/// Update a rule's mutable columns in place (matched by id); `project_id` is left untouched.
/// Returns whether a row matched.
pub(crate) async fn update_limit(pool: &PgPool, r: &LimitRule) -> Result<bool> {
    let (scope_kind, scope_value) = scope_parts(&r.scope);
    let res = sqlx::query(
        "UPDATE limit_rules \
         SET metric = $2, \"window\" = $3, threshold = $4, action = $5, enabled = $6, \
             warn_at = $7, scope_kind = $8, scope_value = $9 \
         WHERE id = $1",
    )
    .bind(r.id.clone())
    .bind(enum_to_str(&r.metric)?)
    .bind(enum_to_str(&r.window)?)
    .bind(r.threshold)
    .bind(enum_to_str(&r.action)?)
    .bind(r.enabled as i64)
    .bind(r.warn_at)
    .bind(scope_kind)
    .bind(scope_value)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(res.rows_affected() > 0)
}

pub(crate) async fn delete_limit(pool: &PgPool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM limit_rules WHERE id = $1")
        .bind(id.to_string())
        .execute(pool)
        .await
        .map_err(pgerr)?;
    Ok(res.rows_affected() > 0)
}

// --- row converters ---------------------------------------------------------

fn project_from_row(row: &PgRow) -> Result<Project> {
    let redaction: String = row.try_get(3).map_err(pgerr)?;
    let created_at: String = row.try_get(5).map_err(pgerr)?;
    Ok(Project {
        id: row.try_get(0).map_err(pgerr)?,
        name: row.try_get(1).map_err(pgerr)?,
        enabled: row.try_get::<i64, _>(2).map_err(pgerr)? != 0,
        redaction: parse_enum::<Redaction>(&redaction),
        collective_opt_in: row.try_get::<i64, _>(4).map_err(pgerr)? != 0,
        created_at: parse_ts(&created_at)?,
    })
}

fn api_key_from_row(row: &PgRow) -> Result<ApiKey> {
    let created_at: String = row.try_get(5).map_err(pgerr)?;
    let last_used: Option<String> = row.try_get(6).map_err(pgerr)?;
    Ok(ApiKey {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        name: row.try_get(2).map_err(pgerr)?,
        prefix: row.try_get(3).map_err(pgerr)?,
        key_hash: row.try_get(4).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
        last_used_at: match last_used {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        revoked: row.try_get::<i64, _>(7).map_err(pgerr)? != 0,
    })
}

fn limit_rule_from_row(row: &PgRow) -> Result<LimitRule> {
    let metric: String = row.try_get(2).map_err(pgerr)?;
    let window: String = row.try_get(3).map_err(pgerr)?;
    let action: String = row.try_get(5).map_err(pgerr)?;
    Ok(LimitRule {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        metric: parse_enum(&metric),
        window: parse_enum(&window),
        threshold: row.try_get(4).map_err(pgerr)?,
        action: parse_enum(&action),
        enabled: row.try_get::<i64, _>(6).map_err(pgerr)? != 0,
        warn_at: row.try_get(7).map_err(pgerr)?,
        scope: match (
            row.try_get::<Option<String>, _>(8).map_err(pgerr)?,
            row.try_get::<Option<String>, _>(9).map_err(pgerr)?,
        ) {
            (Some(kind), Some(value)) => LimitScope::from_parts(&kind, value),
            _ => None,
        },
    })
}
