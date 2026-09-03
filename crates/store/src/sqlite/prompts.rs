//! Prompt registry: named prompts, their label→version pointers, and immutable versions.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{CanaryPolicy, LabelChange, Prompt, PromptVersion};

use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

const PROMPT_COLS: &str =
    "id, project_id, name, benchmark_id, labels, created_at, updated_at, canary, label_history";
const VERSION_COLS: &str = "id, prompt_id, version, content, config, note, created_at";

pub(super) fn create(conn: &Connection, p: &Prompt) -> Result<()> {
    let labels = serde_json::to_string(&p.labels)?;
    conn.execute(
        "INSERT INTO prompts (id, project_id, name, benchmark_id, labels, created_at, updated_at, \
          canary, label_history) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            p.id,
            p.project_id,
            p.name,
            p.benchmark_id,
            labels,
            fmt_ts(p.created_at),
            fmt_ts(p.updated_at),
            canary_json(p)?,
            history_json(p)?,
        ],
    )?;
    Ok(())
}

pub(super) fn update(conn: &Connection, p: &Prompt) -> Result<()> {
    let labels = serde_json::to_string(&p.labels)?;
    conn.execute(
        "UPDATE prompts SET benchmark_id = ?2, labels = ?3, updated_at = ?4, canary = ?5, \
          label_history = ?6 WHERE id = ?1",
        params![
            p.id,
            p.benchmark_id,
            labels,
            fmt_ts(p.updated_at),
            canary_json(p)?,
            history_json(p)?,
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, project: &str, name: &str) -> Result<Option<Prompt>> {
    let sql = format!("SELECT {PROMPT_COLS} FROM prompts WHERE project_id = ?1 AND name = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_row(params![project, name], map_prompt)
        .optional()?;
    raw.map(prompt_from_raw).transpose()
}

pub(super) fn get_by_id(
    conn: &Connection,
    project: Option<&str>,
    id: &str,
) -> Result<Option<Prompt>> {
    let sql = format!(
        "SELECT {PROMPT_COLS} FROM prompts WHERE id = ?1{}",
        super::scope_and(2)
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_row(params![id, project], map_prompt)
        .optional()?;
    raw.map(prompt_from_raw).transpose()
}

pub(super) fn list(conn: &Connection, project: &str) -> Result<Vec<Prompt>> {
    let sql =
        format!("SELECT {PROMPT_COLS} FROM prompts WHERE project_id = ?1 ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_prompt)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(prompt_from_raw).collect()
}

pub(super) fn create_version(conn: &Connection, v: &PromptVersion) -> Result<()> {
    let config = json_or_null(&v.config)?;
    conn.execute(
        "INSERT INTO prompt_versions (id, prompt_id, version, content, config, note, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            v.id,
            v.prompt_id,
            v.version as i64,
            v.content,
            config,
            v.note,
            fmt_ts(v.created_at),
        ],
    )?;
    Ok(())
}

/// `prompt_versions` carries no `project_id` of its own, so the tenant filter rides the parent
/// prompt: a foreign prompt id is simply not found.
pub(super) fn get_version(
    conn: &Connection,
    project: Option<&str>,
    prompt_id: &str,
    version: u32,
) -> Result<Option<PromptVersion>> {
    let sql = format!(
        "SELECT {VERSION_COLS} FROM prompt_versions WHERE prompt_id = ?1 AND version = ?2 \
           AND (?3 IS NULL OR EXISTS \
                (SELECT 1 FROM prompts p WHERE p.id = prompt_id AND p.project_id = ?3))"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_row(params![prompt_id, version as i64, project], map_version)
        .optional()?;
    raw.map(version_from_raw).transpose()
}

pub(super) fn list_versions(
    conn: &Connection,
    project: Option<&str>,
    prompt_id: &str,
) -> Result<Vec<PromptVersion>> {
    let sql = format!(
        "SELECT {VERSION_COLS} FROM prompt_versions WHERE prompt_id = ?1 \
           AND (?2 IS NULL OR EXISTS \
                (SELECT 1 FROM prompts p WHERE p.id = prompt_id AND p.project_id = ?2)) \
         ORDER BY version DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![prompt_id, project], map_version)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(version_from_raw).collect()
}

/// The canary policy as stored: `NULL` for a prompt with none, so an existing row is not given a
/// policy it never had (which would make the sweep act on it).
fn canary_json(p: &Prompt) -> Result<Option<String>> {
    match &p.canary {
        Some(c) => Ok(Some(serde_json::to_string(c)?)),
        None => Ok(None),
    }
}

/// The label ledger as stored: `NULL` for an empty one, so "no moves recorded" and "[]" are the
/// same fact in one spelling.
fn history_json(p: &Prompt) -> Result<Option<String>> {
    match p.label_history.is_empty() {
        true => Ok(None),
        false => Ok(Some(serde_json::to_string(&p.label_history)?)),
    }
}

type PromptRaw = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn map_prompt(row: &Row) -> rusqlite::Result<PromptRaw> {
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
    ))
}

fn prompt_from_raw(r: PromptRaw) -> Result<Prompt> {
    let labels: BTreeMap<String, u32> = serde_json::from_str(&r.4)?;
    let canary: Option<CanaryPolicy> = r.7.as_deref().map(serde_json::from_str).transpose()?;
    let label_history: Vec<LabelChange> = match r.8.as_deref() {
        Some(s) => serde_json::from_str(s)?,
        None => Vec::new(),
    };
    Ok(Prompt {
        id: r.0,
        project_id: r.1,
        name: r.2,
        benchmark_id: r.3,
        labels,
        canary,
        label_history,
        created_at: parse_ts(&r.5)?,
        updated_at: parse_ts(&r.6)?,
    })
}

type VersionRaw = (
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    String,
);

fn map_version(row: &Row) -> rusqlite::Result<VersionRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn version_from_raw(r: VersionRaw) -> Result<PromptVersion> {
    Ok(PromptVersion {
        id: r.0,
        prompt_id: r.1,
        version: r.2 as u32,
        content: r.3,
        config: val_or_null(r.4)?,
        note: r.5,
        created_at: parse_ts(&r.6)?,
    })
}

#[cfg(test)]
mod cols_tests {
    use super::*;

    #[test]
    fn prompt_cols_match_the_schema_model() {
        use crate::schema::{tables, Dialect};
        assert_eq!(PROMPT_COLS, tables::PROMPTS.select_list(Dialect::Sqlite));
    }

    #[test]
    fn version_cols_match_the_schema_model() {
        use crate::schema::{tables, Dialect};
        assert_eq!(
            VERSION_COLS,
            tables::PROMPT_VERSIONS.select_list(Dialect::Sqlite)
        );
    }
}
