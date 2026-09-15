//! The use-case registry, and the one read that makes it a monitoring layer rather than a table:
//! what the events *actually* called themselves.

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{UseCase, UseCaseKind, UseCaseStatus};

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "id, project_id, key, name, description, kind, status, component, \
                    expected_models, owner, created_at, updated_at";

/// Create or replace by `(project_id, key)`.
///
/// Upsert rather than insert because `key` — not `id` — is the identity an operator and an SDK both
/// think in. Registering the same call site twice is a correction, not a conflict, and making it a
/// 409 would mean the obvious way to fix a typo'd description is to delete the row that the events
/// are already attributing to.
pub(super) fn upsert(conn: &Connection, u: &UseCase) -> Result<()> {
    let models = serde_json::to_string(&u.expected_models)?;
    conn.execute(
        "INSERT INTO use_cases (id, project_id, key, name, description, kind, status, component, \
          expected_models, owner, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
         ON CONFLICT(project_id, key) DO UPDATE SET \
          name = excluded.name, description = excluded.description, kind = excluded.kind, \
          status = excluded.status, component = excluded.component, \
          expected_models = excluded.expected_models, owner = excluded.owner, \
          updated_at = excluded.updated_at",
        params![
            u.id,
            u.project_id,
            u.key,
            u.name,
            u.description,
            u.kind.as_str(),
            u.status.as_str(),
            u.component,
            models,
            u.owner,
            fmt_ts(u.created_at),
            fmt_ts(u.updated_at),
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, project: &str, key: &str) -> Result<Option<UseCase>> {
    let sql = format!("SELECT {COLS} FROM use_cases WHERE project_id = ?1 AND key = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![project, key], map_row).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn list(conn: &Connection, project: &str) -> Result<Vec<UseCase>> {
    let sql = format!("SELECT {COLS} FROM use_cases WHERE project_id = ?1 ORDER BY key");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt
        .query_map(params![project], map_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter().map(from_raw).collect()
}

pub(super) fn delete(conn: &Connection, project: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM use_cases WHERE project_id = ?1 AND key = ?2",
        params![project, key],
    )?;
    Ok(n > 0)
}

/// Every distinct `events.name` this project has actually emitted, with its call count and the
/// models it ran on.
///
/// This is the other half of the registry. Declared-minus-observed is a use case that has gone
/// quiet; observed-minus-declared is **shadow usage** — a call site nobody registered, or a typo
/// splitting one use case's cost across two rollup keys. Neither is visible from the registry alone,
/// and neither is visible from the events alone; the report is the difference.
///
/// Events with no `name` are counted under the empty key rather than dropped, because "calls that
/// declared nothing at all" is the largest and most actionable bucket in a project that has not
/// instrumented yet — silently omitting it would make an uninstrumented project look complete.
pub(super) fn observed(
    conn: &Connection,
    project: &str,
    since: Option<&str>,
) -> Result<Vec<(String, u64, Vec<String>)>> {
    let mut sql = String::from(
        "SELECT COALESCE(name, '') AS n, COUNT(*) AS c, \
         GROUP_CONCAT(DISTINCT model) AS models \
         FROM events WHERE project_id = ?1",
    );
    if since.is_some() {
        sql.push_str(" AND ts >= ?2");
    }
    sql.push_str(" GROUP BY n ORDER BY c DESC");
    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &Row<'_>| {
        let models: Option<String> = r.get(2)?;
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?.max(0) as u64,
            models
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>(),
        ))
    };
    let rows: rusqlite::Result<Vec<(String, u64, Vec<String>)>> = match since {
        Some(s) => stmt.query_map(params![project, s], map)?.collect(),
        None => stmt.query_map(params![project], map)?.collect(),
    };
    rows.map_err(Into::into)
}

/// A row as SQLite hands it over: timestamps still strings, because parsing them can fail with a
/// `StoreError` and `query_map`'s mapper may only fail with a `rusqlite::Error`.
type Raw = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn map_row(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
    ))
}

fn from_raw(raw: Raw) -> Result<UseCase> {
    let (id, project_id, key, name, description, kind, status, component, models, owner, c, u) =
        raw;
    Ok(UseCase {
        id,
        project_id,
        key,
        name,
        description,
        // A stored value outside the vocabulary degrades to `Other` / `Active` rather than failing
        // the read: a row written by a newer build must not make the registry unlistable on an
        // older one.
        kind: serde_json::from_value(serde_json::Value::String(kind)).unwrap_or(UseCaseKind::Other),
        status: serde_json::from_value(serde_json::Value::String(status))
            .unwrap_or(UseCaseStatus::Active),
        component,
        expected_models: models
            .and_then(|m| serde_json::from_str(&m).ok())
            .unwrap_or_default(),
        owner,
        created_at: parse_ts(&c)?,
        updated_at: parse_ts(&u)?,
    })
}
