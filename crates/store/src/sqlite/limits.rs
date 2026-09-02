//! Limit rules: create / list / get / update / delete over the `limit_rules` table.
//!
//! Split out of `projects` so the budget-limit domain owns one submodule (per the store layout in
//! CLAUDE.md). Every function is a free function over a locked `&Connection`, so the whole
//! create-then-evaluate admission path stays inside `SqliteStore`'s single critical section.

use rusqlite::{params, Connection, OptionalExtension, Row};

use chrono::{DateTime, Utc};

use lighttrack_core::{
    Escalation, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, Threshold,
};

use crate::codec::{enum_to_str, fmt_ts, parse_enum, parse_ts};
use crate::Result;

/// The columns a rule row exposes, in the order [`map_limit`] reads them.
const COLS: &str = "id, project_id, metric, window, threshold, action, enabled, warn_at, \
     scope_kind, scope_value, threshold_json, escalation_json, escalated_until, origin, expires_at";

/// Split a threshold into its `(REAL, JSON)` column pair.
///
/// The original `threshold REAL NOT NULL` column stays the home of a plain `Fixed` cap — every row
/// written before derived thresholds existed reads back byte-identically, and a DBA looking at the
/// table still sees the number. Anything richer goes to `threshold_json`, whose presence is what
/// [`limit_from_raw`] keys on.
fn threshold_parts(t: &Threshold) -> Result<(f64, Option<String>)> {
    match t {
        Threshold::Fixed(v) => Ok((*v, None)),
        other => Ok((0.0, Some(serde_json::to_string(other)?))),
    }
}

fn escalation_json(e: &Option<Escalation>) -> Result<Option<String>> {
    Ok(match e {
        None => None,
        Some(e) => Some(serde_json::to_string(e)?),
    })
}

pub(super) fn create(conn: &Connection, r: &LimitRule) -> Result<()> {
    let (scope_kind, scope_value) = scope_parts(&r.scope);
    let (threshold, threshold_json) = threshold_parts(&r.threshold)?;
    conn.execute(
        "INSERT INTO limit_rules \
         (id, project_id, metric, window, threshold, action, enabled, warn_at, scope_kind, \
          scope_value, threshold_json, escalation_json, escalated_until, origin, expires_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            r.id,
            r.project_id,
            enum_to_str(&r.metric)?,
            enum_to_str(&r.window)?,
            threshold,
            enum_to_str(&r.action)?,
            r.enabled as i64,
            r.warn_at,
            scope_kind,
            scope_value,
            threshold_json,
            escalation_json(&r.escalation)?,
            r.escalated_until.map(fmt_ts),
            r.origin,
            r.expires_at.map(fmt_ts),
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
    let raws = stmt
        .query_map(params![project], map_limit)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(limit_from_raw).collect()
}

pub(super) fn get(conn: &Connection, project: Option<&str>, id: &str) -> Result<Option<LimitRule>> {
    let sql = format!(
        "SELECT {COLS} FROM limit_rules WHERE id = ?1{}",
        super::scope_and(2)
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id, project], map_limit)
        .optional()?
        .map(limit_from_raw)
        .transpose()
}

/// Update a rule's mutable columns in place (matched by id); `project_id` is left untouched. Returns
/// whether a row matched.
pub(super) fn update(conn: &Connection, project: Option<&str>, r: &LimitRule) -> Result<bool> {
    let (scope_kind, scope_value) = scope_parts(&r.scope);
    let (threshold, threshold_json) = threshold_parts(&r.threshold)?;
    let sql = format!(
        "UPDATE limit_rules \
         SET metric = ?2, window = ?3, threshold = ?4, action = ?5, enabled = ?6, warn_at = ?7, \
             scope_kind = ?8, scope_value = ?9, threshold_json = ?10, escalation_json = ?11, \
             escalated_until = ?12, origin = ?13, expires_at = ?14 \
         WHERE id = ?1{}",
        super::scope_and(15)
    );
    let n = conn.execute(
        &sql,
        params![
            r.id,
            enum_to_str(&r.metric)?,
            enum_to_str(&r.window)?,
            threshold,
            enum_to_str(&r.action)?,
            r.enabled as i64,
            r.warn_at,
            scope_kind,
            scope_value,
            threshold_json,
            escalation_json(&r.escalation)?,
            r.escalated_until.map(fmt_ts),
            r.origin,
            r.expires_at.map(fmt_ts),
            project,
        ],
    )?;
    Ok(n > 0)
}

pub(super) fn delete(conn: &Connection, project: Option<&str>, id: &str) -> Result<bool> {
    let sql = format!(
        "DELETE FROM limit_rules WHERE id = ?1{}",
        super::scope_and(2)
    );
    let n = conn.execute(&sql, params![id, project])?;
    Ok(n > 0)
}

/// The columns as SQLite hands them over, before any domain decoding.
///
/// Split from [`limit_from_raw`] because decoding a closed vocabulary can now FAIL, and a
/// `rusqlite::Result` cannot carry that failure — the same raw-then-decode shape the jobs mapper
/// already uses. A rule whose `metric`/`window`/`action` is outside the vocabulary is a defect to
/// surface, not a value to coerce: the tempting `unwrap_or_default()` would turn a drift bug into a
/// silently different cap.
type LimitRaw = (
    String,
    String,
    String,
    String,
    f64,
    String,
    i64,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn map_limit(row: &Row) -> rusqlite::Result<LimitRaw> {
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
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
    ))
}

fn limit_from_raw(r: LimitRaw) -> Result<LimitRule> {
    Ok(LimitRule {
        id: r.0,
        project_id: r.1,
        metric: parse_enum::<LimitMetric>("metric", &r.2)?,
        window: parse_enum::<LimitWindow>("window", &r.3)?,
        // `threshold_json` wins when present; its absence is the pre-derived-threshold row, whose
        // REAL column is the whole story.
        threshold: match r.10.as_deref() {
            Some(j) => serde_json::from_str(j)?,
            None => Threshold::Fixed(r.4),
        },
        action: parse_enum::<LimitAction>("action", &r.5)?,
        enabled: r.6 != 0,
        warn_at: r.7,
        scope: match (r.8, r.9) {
            (Some(kind), Some(value)) => LimitScope::from_parts(&kind, value),
            _ => None,
        },
        escalation: match r.11.as_deref() {
            Some(j) => Some(serde_json::from_str(j)?),
            None => None,
        },
        escalated_until: opt_ts(r.12.as_deref())?,
        origin: r.13,
        expires_at: opt_ts(r.14.as_deref())?,
    })
}

fn opt_ts(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    s.map(parse_ts).transpose()
}
