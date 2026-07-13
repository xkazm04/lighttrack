//! Projects and API keys. (Limit rules live in the sibling [`super::limits`] submodule.)

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{ApiKey, Project, Redaction};

use crate::codec::{enum_to_str, fmt_ts, parse_enum, parse_ts};
use crate::Result;

// --- projects ---

pub(super) fn create(conn: &Connection, p: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (id, name, enabled, redaction, created_at) VALUES (?1,?2,?3,?4,?5)",
        params![p.id, p.name, p.enabled as i64, enum_to_str(&p.redaction)?, fmt_ts(p.created_at)],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Project>> {
    let mut stmt =
        conn.prepare("SELECT id, name, enabled, redaction, created_at FROM projects WHERE id = ?1")?;
    let raw = stmt.query_row(params![id], map_project).optional()?;
    raw.map(project_from_raw).transpose()
}

pub(super) fn list(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, enabled, redaction, created_at FROM projects ORDER BY created_at DESC",
    )?;
    let raws = stmt.query_map([], map_project)?.collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(project_from_raw).collect()
}

type ProjectRaw = (String, String, i64, String, String);

fn map_project(row: &Row) -> rusqlite::Result<ProjectRaw> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn project_from_raw(r: ProjectRaw) -> Result<Project> {
    Ok(Project {
        id: r.0,
        name: r.1,
        enabled: r.2 != 0,
        redaction: parse_enum::<Redaction>(&r.3),
        created_at: parse_ts(&r.4)?,
    })
}

// --- api keys ---

pub(super) fn create_key(conn: &Connection, k: &ApiKey) -> Result<()> {
    conn.execute(
        "INSERT INTO api_keys \
         (id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            k.id,
            k.project_id,
            k.name,
            k.prefix,
            k.key_hash,
            fmt_ts(k.created_at),
            k.last_used_at.map(fmt_ts),
            k.revoked as i64,
        ],
    )?;
    Ok(())
}

pub(super) fn find_key_by_prefix(conn: &Connection, prefix: &str) -> Result<Option<ApiKey>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked \
         FROM api_keys WHERE prefix = ?1",
    )?;
    let raw = stmt.query_row(params![prefix], map_key).optional()?;
    raw.map(key_from_raw).transpose()
}

pub(super) fn touch_key(conn: &Connection, id: &str, when: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1",
        params![id, fmt_ts(when)],
    )?;
    Ok(())
}

type ApiKeyRaw = (String, String, String, String, String, String, Option<String>, i64);

fn map_key(row: &Row) -> rusqlite::Result<ApiKeyRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn key_from_raw(r: ApiKeyRaw) -> Result<ApiKey> {
    Ok(ApiKey {
        id: r.0,
        project_id: r.1,
        name: r.2,
        prefix: r.3,
        key_hash: r.4,
        created_at: parse_ts(&r.5)?,
        last_used_at: match r.6 {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        revoked: r.7 != 0,
    })
}
