//! Prompt registry: named prompts, their label→version pointers, and immutable versions.
//!
//! The gap this closes: the registry was `Unsupported` on Postgres, so on a managed deployment
//! (Neon, RDS, Cloud SQL) `/v1/projects/:id/prompts*` answered 501 — meaning the promotion gate,
//! the one place a prompt edit becomes a measurable quality step, did not exist where the product
//! actually runs. Mirrors `store/src/sqlite/prompts.rs` column for column.

use std::collections::BTreeMap;

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{CanaryPolicy, LabelChange, Prompt, PromptVersion};
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

const PROMPT_COLS: &str =
    "id, project_id, name, benchmark_id, labels, created_at, updated_at, canary, label_history";
const VERSION_COLS: &str = "id, prompt_id, version, content, config, note, created_at";

pub(crate) async fn create(pool: &PgPool, p: &Prompt) -> Result<()> {
    sqlx::query(
        "INSERT INTO prompts (id, project_id, name, benchmark_id, labels, created_at, updated_at, \
          canary, label_history) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(p.id.clone())
    .bind(p.project_id.clone())
    .bind(p.name.clone())
    .bind(p.benchmark_id.clone())
    .bind(serde_json::to_string(&p.labels)?)
    .bind(fmt_ts(p.created_at))
    .bind(fmt_ts(p.updated_at))
    .bind(canary_json(p)?)
    .bind(history_json(p)?)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn update(pool: &PgPool, p: &Prompt) -> Result<()> {
    sqlx::query(
        "UPDATE prompts SET benchmark_id = $2, labels = $3, updated_at = $4, canary = $5, \
          label_history = $6 WHERE id = $1",
    )
    .bind(p.id.clone())
    .bind(p.benchmark_id.clone())
    .bind(serde_json::to_string(&p.labels)?)
    .bind(fmt_ts(p.updated_at))
    .bind(canary_json(p)?)
    .bind(history_json(p)?)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, project: &str, name: &str) -> Result<Option<Prompt>> {
    let row = sqlx::query(&format!(
        "SELECT {PROMPT_COLS} FROM prompts WHERE project_id = $1 AND name = $2"
    ))
    .bind(project.to_string())
    .bind(name.to_string())
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(prompt_from_row).transpose()
}

pub(crate) async fn get_by_id(pool: &PgPool, id: &str) -> Result<Option<Prompt>> {
    let row = sqlx::query(&format!("SELECT {PROMPT_COLS} FROM prompts WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(prompt_from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool, project: &str) -> Result<Vec<Prompt>> {
    let rows = sqlx::query(&format!(
        "SELECT {PROMPT_COLS} FROM prompts WHERE project_id = $1 ORDER BY created_at DESC"
    ))
    .bind(project.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(prompt_from_row).collect()
}

pub(crate) async fn create_version(pool: &PgPool, v: &PromptVersion) -> Result<()> {
    // `config` is stored as text-or-NULL exactly as SQLite does, so a version written by one
    // backend and read by the other round-trips to the same `Value`.
    let config = match &v.config {
        serde_json::Value::Null => None,
        other => Some(serde_json::to_string(other)?),
    };
    sqlx::query(
        "INSERT INTO prompt_versions (id, prompt_id, version, content, config, note, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(v.id.clone())
    .bind(v.prompt_id.clone())
    .bind(v.version as i32)
    .bind(v.content.clone())
    .bind(config)
    .bind(v.note.clone())
    .bind(fmt_ts(v.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get_version(
    pool: &PgPool,
    prompt_id: &str,
    version: u32,
) -> Result<Option<PromptVersion>> {
    let row = sqlx::query(&format!(
        "SELECT {VERSION_COLS} FROM prompt_versions WHERE prompt_id = $1 AND version = $2"
    ))
    .bind(prompt_id.to_string())
    .bind(version as i32)
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(version_from_row).transpose()
}

/// Newest version first — a reversed order would serve a stale prompt to every runtime fetch that
/// asks for "the latest", which is why the conformance suite pins it.
pub(crate) async fn list_versions(pool: &PgPool, prompt_id: &str) -> Result<Vec<PromptVersion>> {
    let rows = sqlx::query(&format!(
        "SELECT {VERSION_COLS} FROM prompt_versions WHERE prompt_id = $1 ORDER BY version DESC"
    ))
    .bind(prompt_id.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(version_from_row).collect()
}

/// The canary policy as stored: NULL for a prompt with none, so an existing row is never given a
/// policy it did not have — which the sweep would then act on.
fn canary_json(p: &Prompt) -> Result<Option<String>> {
    match &p.canary {
        Some(c) => Ok(Some(serde_json::to_string(c)?)),
        None => Ok(None),
    }
}

/// The label ledger as stored: NULL for an empty one, so "no moves recorded" has one spelling.
fn history_json(p: &Prompt) -> Result<Option<String>> {
    match p.label_history.is_empty() {
        true => Ok(None),
        false => Ok(Some(serde_json::to_string(&p.label_history)?)),
    }
}

fn prompt_from_row(row: &PgRow) -> Result<Prompt> {
    let labels: String = row.try_get(4).map_err(pgerr)?;
    let created_at: String = row.try_get(5).map_err(pgerr)?;
    let updated_at: String = row.try_get(6).map_err(pgerr)?;
    let canary: Option<String> = row.try_get(7).map_err(pgerr)?;
    let history: Option<String> = row.try_get(8).map_err(pgerr)?;
    Ok(Prompt {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        name: row.try_get(2).map_err(pgerr)?,
        benchmark_id: row.try_get(3).map_err(pgerr)?,
        labels: serde_json::from_str::<BTreeMap<String, u32>>(&labels)?,
        canary: canary
            .as_deref()
            .map(serde_json::from_str::<CanaryPolicy>)
            .transpose()?,
        label_history: match history.as_deref() {
            Some(h) => serde_json::from_str::<Vec<LabelChange>>(h)?,
            None => Vec::new(),
        },
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn version_from_row(row: &PgRow) -> Result<PromptVersion> {
    let version: i32 = row.try_get(2).map_err(pgerr)?;
    let config: Option<String> = row.try_get(4).map_err(pgerr)?;
    let created_at: String = row.try_get(6).map_err(pgerr)?;
    Ok(PromptVersion {
        id: row.try_get(0).map_err(pgerr)?,
        prompt_id: row.try_get(1).map_err(pgerr)?,
        version: version.max(0) as u32,
        content: row.try_get(3).map_err(pgerr)?,
        config: match config {
            Some(s) => serde_json::from_str(&s)?,
            None => serde_json::Value::Null,
        },
        note: row.try_get(5).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}
