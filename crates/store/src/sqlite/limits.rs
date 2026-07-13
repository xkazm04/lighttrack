//! Limit rules: create / list / get / update / delete over the `limit_rules` table.
//!
//! Split out of `projects` so the budget-limit domain owns one submodule (per the store layout in
//! CLAUDE.md). Every function is a free function over a locked `&Connection`, so the whole
//! create-then-evaluate admission path stays inside `SqliteStore`'s single critical section.

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow};

use crate::codec::{enum_to_str, parse_enum};
use crate::Result;

/// The columns a rule row exposes, in the order [`map_limit`] reads them.
const COLS: &str =
    "id, project_id, metric, window, threshold, action, enabled, warn_at, scope_kind, scope_value";

pub(super) fn create(conn: &Connection, r: &LimitRule) -> Result<()> {
    let (scope_kind, scope_value) = scope_parts(&r.scope);
    conn.execute(
        "INSERT INTO limit_rules \
         (id, project_id, metric, window, threshold, action, enabled, warn_at, scope_kind, scope_value) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            r.id,
            r.project_id,
            enum_to_str(&r.metric)?,
            enum_to_str(&r.window)?,
            r.threshold,
            enum_to_str(&r.action)?,
            r.enabled as i64,
            r.warn_at,
            scope_kind,
            scope_value,
        ],
    )?;
    Ok(())
}

/// Split an optional scope into its `(kind, value)` column pair (both `None` when unscoped).
fn scope_parts(scope: &Option<LimitScope>) -> (Option<&'static str>, Option<String>) {
    match scope {
        None => (None, None),
        Some(s) => (Some(s.kind_str()), Some(s.value().to_string())),
    }
}

pub(super) fn list(conn: &Connection, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
    let sql = if only_enabled {
        format!("SELECT {COLS} FROM limit_rules WHERE project_id = ?1 AND enabled = 1")
    } else {
        format!("SELECT {COLS} FROM limit_rules WHERE project_id = ?1")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![project], map_limit)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<LimitRule>> {
    let sql = format!("SELECT {COLS} FROM limit_rules WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id], map_limit).optional().map_err(Into::into)
}

/// Update a rule's mutable columns in place (matched by id); `project_id` is left untouched. Returns
/// whether a row matched.
pub(super) fn update(conn: &Connection, r: &LimitRule) -> Result<bool> {
    let (scope_kind, scope_value) = scope_parts(&r.scope);
    let n = conn.execute(
        "UPDATE limit_rules \
         SET metric = ?2, window = ?3, threshold = ?4, action = ?5, enabled = ?6, warn_at = ?7, \
             scope_kind = ?8, scope_value = ?9 \
         WHERE id = ?1",
        params![
            r.id,
            enum_to_str(&r.metric)?,
            enum_to_str(&r.window)?,
            r.threshold,
            enum_to_str(&r.action)?,
            r.enabled as i64,
            r.warn_at,
            scope_kind,
            scope_value,
        ],
    )?;
    Ok(n > 0)
}

pub(super) fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM limit_rules WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

fn map_limit(row: &Row) -> rusqlite::Result<LimitRule> {
    Ok(LimitRule {
        id: row.get(0)?,
        project_id: row.get(1)?,
        metric: parse_enum::<LimitMetric>(&row.get::<_, String>(2)?),
        window: parse_enum::<LimitWindow>(&row.get::<_, String>(3)?),
        threshold: row.get(4)?,
        action: parse_enum::<LimitAction>(&row.get::<_, String>(5)?),
        enabled: row.get::<_, i64>(6)? != 0,
        warn_at: row.get(7)?,
        scope: match (row.get::<_, Option<String>>(8)?, row.get::<_, Option<String>>(9)?) {
            (Some(kind), Some(value)) => LimitScope::from_parts(&kind, value),
            _ => None,
        },
    })
}
