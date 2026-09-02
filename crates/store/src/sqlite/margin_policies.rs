//! Margin guardrail policies: create / list / get / delete over the `margin_policies` table.
//!
//! The `trigger` and `action` shapes are open-ended sum types (a trigger gains variants as new
//! margin signals land), so they are stored as JSON in one column each rather than shredded into
//! `kind` + `value` pairs that would need a migration per variant. This is config read once per
//! sweep, never on the ingest hot path, so the parse cost is irrelevant and the schema stability is
//! worth a lot.

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{MarginPolicy, PolicyAction, PolicyTrigger};

use crate::{Result, StoreError};

const COLS: &str =
    "id, project_id, trigger_json, min_cost_usd, action_json, cooldown_secs, expiry_secs, enabled";

pub(super) fn create(conn: &Connection, p: &MarginPolicy) -> Result<()> {
    conn.execute(
        "INSERT INTO margin_policies \
         (id, project_id, trigger_json, min_cost_usd, action_json, cooldown_secs, expiry_secs, enabled) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            p.id,
            p.project_id,
            serde_json::to_string(&p.trigger)?,
            p.min_cost_usd,
            serde_json::to_string(&p.action)?,
            p.cooldown_secs as i64,
            p.expiry_secs as i64,
            p.enabled as i64,
        ],
    )?;
    Ok(())
}

pub(super) fn list(
    conn: &Connection,
    project: &str,
    only_enabled: bool,
) -> Result<Vec<MarginPolicy>> {
    let sql = if only_enabled {
        format!("SELECT {COLS} FROM margin_policies WHERE project_id = ?1 AND enabled = 1")
    } else {
        format!("SELECT {COLS} FROM margin_policies WHERE project_id = ?1")
    };
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<MarginPolicy>> {
    let sql = format!("SELECT {COLS} FROM margin_policies WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id], map_raw)
        .optional()?
        .map(from_raw)
        .transpose()
}

pub(super) fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM margin_policies WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

type PolicyRaw = (String, String, String, f64, String, i64, i64, i64);

fn map_raw(row: &Row) -> rusqlite::Result<PolicyRaw> {
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

/// Decode a stored policy. A row whose `trigger`/`action` JSON is outside the vocabulary is a defect
/// to surface, not a value to coerce: silently defaulting a trigger would change what a guardrail
/// fires on.
fn from_raw(r: PolicyRaw) -> Result<MarginPolicy> {
    let trigger: PolicyTrigger = serde_json::from_str(&r.2).map_err(|e| {
        StoreError::Other(format!(
            "margin policy '{}' has an unreadable trigger: {e}",
            r.0
        ))
    })?;
    let action: PolicyAction = serde_json::from_str(&r.4).map_err(|e| {
        StoreError::Other(format!(
            "margin policy '{}' has an unreadable action: {e}",
            r.0
        ))
    })?;
    Ok(MarginPolicy {
        id: r.0,
        project_id: r.1,
        trigger,
        min_cost_usd: r.3,
        action,
        cooldown_secs: r.5.max(0) as u64,
        expiry_secs: r.6.max(0) as u64,
        enabled: r.7 != 0,
    })
}
