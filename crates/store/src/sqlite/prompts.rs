//! Prompt registry: named prompts, their label→version pointers, and immutable versions.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{Prompt, PromptVersion};

use crate::codec::{fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::Result;

const PROMPT_COLS: &str = "id, project_id, name, benchmark_id, labels, created_at, updated_at";
const VERSION_COLS: &str = "id, prompt_id, version, content, config, note, created_at";

pub(super) fn create(conn: &Connection, p: &Prompt) -> Result<()> {
    let labels = serde_json::to_string(&p.labels)?;
    conn.execute(
        "INSERT INTO prompts (id, project_id, name, benchmark_id, labels, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            p.id,
            p.project_id,
            p.name,
            p.benchmark_id,
            labels,
            fmt_ts(p.created_at),
            fmt_ts(p.updated_at),
        ],
    )?;
    Ok(())
}

pub(super) fn update(conn: &Connection, p: &Prompt) -> Result<()> {
    let labels = serde_json::to_string(&p.labels)?;
    conn.execute(
        "UPDATE prompts SET benchmark_id = ?2, labels = ?3, updated_at = ?4 WHERE id = ?1",
        params![p.id, p.benchmark_id, labels, fmt_ts(p.updated_at)],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, project: &str, name: &str) -> Result<Option<Prompt>> {
    let sql = format!("SELECT {PROMPT_COLS} FROM prompts WHERE project_id = ?1 AND name = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![project, name], map_prompt).optional()?;
    raw.map(prompt_from_raw).transpose()
}

pub(super) fn get_by_id(conn: &Connection, id: &str) -> Result<Option<Prompt>> {
    let sql = format!("SELECT {PROMPT_COLS} FROM prompts WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_prompt).optional()?;
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

pub(super) fn get_version(
    conn: &Connection,
    prompt_id: &str,
    version: u32,
) -> Result<Option<PromptVersion>> {
    let sql =
        format!("SELECT {VERSION_COLS} FROM prompt_versions WHERE prompt_id = ?1 AND version = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_row(params![prompt_id, version as i64], map_version)
        .optional()?;
    raw.map(version_from_raw).transpose()
}

pub(super) fn list_versions(conn: &Connection, prompt_id: &str) -> Result<Vec<PromptVersion>> {
    let sql = format!(
        "SELECT {VERSION_COLS} FROM prompt_versions WHERE prompt_id = ?1 ORDER BY version DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![prompt_id], map_version)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(version_from_raw).collect()
}

type PromptRaw = (String, String, String, Option<String>, String, String, String);

fn map_prompt(row: &Row) -> rusqlite::Result<PromptRaw> {
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

fn prompt_from_raw(r: PromptRaw) -> Result<Prompt> {
    let labels: BTreeMap<String, u32> = serde_json::from_str(&r.4)?;
    Ok(Prompt {
        id: r.0,
        project_id: r.1,
        name: r.2,
        benchmark_id: r.3,
        labels,
        created_at: parse_ts(&r.5)?,
        updated_at: parse_ts(&r.6)?,
    })
}

type VersionRaw = (String, String, i64, String, Option<String>, Option<String>, String);

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
