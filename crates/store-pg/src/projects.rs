//! Projects, API keys, and limit rules.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{ApiKey, LimitRule, Project, Redaction};
use lighttrack_store::Result;

use crate::util::{enum_to_str, fmt_ts, parse_enum, parse_ts, pgerr};

// --- projects ---------------------------------------------------------------

pub(crate) async fn create(pool: &PgPool, p: &Project) -> Result<()> {
    sqlx::query(
        "INSERT INTO projects (id, name, enabled, redaction, created_at) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(p.id.clone())
    .bind(p.name.clone())
    .bind(p.enabled as i64)
    .bind(enum_to_str(&p.redaction)?)
    .bind(fmt_ts(p.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Project>> {
    let row = sqlx::query("SELECT id, name, enabled, redaction, created_at FROM projects WHERE id = $1")
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(project_from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool) -> Result<Vec<Project>> {
    let rows =
        sqlx::query("SELECT id, name, enabled, redaction, created_at FROM projects ORDER BY created_at DESC")
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

// --- limit rules ------------------------------------------------------------

pub(crate) async fn create_limit(pool: &PgPool, r: &LimitRule) -> Result<()> {
    sqlx::query(
        "INSERT INTO limit_rules (id, project_id, metric, \"window\", threshold, action, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(r.id.clone())
    .bind(r.project_id.clone())
    .bind(enum_to_str(&r.metric)?)
    .bind(enum_to_str(&r.window)?)
    .bind(r.threshold)
    .bind(enum_to_str(&r.action)?)
    .bind(r.enabled as i64)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list_limits(pool: &PgPool, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
    let sql = if only_enabled {
        "SELECT id, project_id, metric, \"window\", threshold, action, enabled \
         FROM limit_rules WHERE project_id = $1 AND enabled = 1"
    } else {
        "SELECT id, project_id, metric, \"window\", threshold, action, enabled \
         FROM limit_rules WHERE project_id = $1"
    };
    let rows = sqlx::query(sql).bind(project.to_string()).fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().map(limit_rule_from_row).collect()
}

// --- row converters ---------------------------------------------------------

fn project_from_row(row: &PgRow) -> Result<Project> {
    let redaction: String = row.try_get(3).map_err(pgerr)?;
    let created_at: String = row.try_get(4).map_err(pgerr)?;
    Ok(Project {
        id: row.try_get(0).map_err(pgerr)?,
        name: row.try_get(1).map_err(pgerr)?,
        enabled: row.try_get::<i64, _>(2).map_err(pgerr)? != 0,
        redaction: parse_enum::<Redaction>(&redaction),
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
        // warn_at / scope: not yet persisted by this backend (handoff) — defaulted so the shared
        // LimitRule constructs; soft-warnings and scoped caps fall back to their Store trait defaults.
        warn_at: None,
        scope: None,
    })
}
