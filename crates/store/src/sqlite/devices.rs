//! `Surface::Devices`: the enrolled relay fleet — who may lease, and what each one can run.
//!
//! Same shape as `api_keys`: a non-secret `key_prefix` for lookup and a salted digest beside it,
//! never the raw key. `capabilities` is a JSON array of action types / `"<ns>/*"` prefixes; the
//! *eligibility* question is answered in Rust over the decoded list rather than in SQL, because the
//! match rule (a namespace prefix must stop at a `/`) is one rule that has to mean the same thing
//! in the lease filter, the enqueue verdict, and every backend — and a `LIKE` pattern rebuilt per
//! backend is how those three quietly drift apart.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{Device, DeviceEligibility};

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "id, project_id, name, key_prefix, key_hash, capabilities, last_seen_at, \
     agent_version, created_at, revoked";

pub(super) fn create(conn: &Connection, d: &Device) -> Result<()> {
    conn.execute(
        &format!("INSERT INTO devices ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
        params![
            d.id,
            d.project_id,
            d.name,
            d.key_prefix,
            d.key_hash,
            encode_caps(&d.capabilities)?,
            d.last_seen_at.map(fmt_ts),
            d.agent_version,
            fmt_ts(d.created_at),
            d.revoked as i64,
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Device>> {
    one(conn, "id = ?1", id)
}

/// Lookup by the presented key's non-secret prefix. Revoked devices are returned rather than
/// filtered: the caller refuses them explicitly, so "revoked" and "no such device" stay different
/// answers to whoever is debugging an authentication failure.
pub(super) fn find_by_key_prefix(conn: &Connection, prefix: &str) -> Result<Option<Device>> {
    one(conn, "key_prefix = ?1", prefix)
}

fn one(conn: &Connection, whr: &str, arg: &str) -> Result<Option<Device>> {
    let sql = format!("SELECT {COLS} FROM devices WHERE {whr}");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![arg], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

/// One project's devices, or every device when `project` is `None`.
///
/// A project filter keeps the operator-wide devices (`project_id IS NULL`) in the result: those
/// serve every project's tasks, so omitting them would show a project an emptier fleet than the one
/// actually running its work.
pub(super) fn list(conn: &Connection, project: Option<&str>) -> Result<Vec<Device>> {
    let sql = format!(
        "SELECT {COLS} FROM devices \
         WHERE (?1 IS NULL OR project_id = ?1 OR project_id IS NULL) \
         ORDER BY created_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

pub(super) fn touch(
    conn: &Connection,
    id: &str,
    capabilities: &[String],
    agent_version: Option<&str>,
) -> Result<()> {
    // The reported capability set replaces the stored one only when the device actually sent one:
    // a pre-M18 agent advertises nothing, and letting that blank the row would silently widen the
    // device to "everything" behind the operator's back.
    let caps = if capabilities.is_empty() {
        None
    } else {
        Some(encode_caps(capabilities)?)
    };
    conn.execute(
        "UPDATE devices SET last_seen_at = ?2, \
             capabilities = COALESCE(?3, capabilities), \
             agent_version = COALESCE(?4, agent_version) \
         WHERE id = ?1",
        params![id, fmt_ts(Utc::now()), caps, agent_version],
    )?;
    Ok(())
}

pub(super) fn revoke(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("UPDATE devices SET revoked = 1 WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// How much of the fleet serves `action_type`. A whole-table read on purpose: the devices table is
/// a fleet, not a workload — tens of rows at most — and doing the match in Rust keeps one authority
/// for what a capability covers ([`lighttrack_core::capability_matches`]).
pub(super) fn count_eligible(conn: &Connection, action_type: &str) -> Result<DeviceEligibility> {
    let devices = list(conn, None)?;
    Ok(DeviceEligibility::count(&devices, action_type))
}

fn encode_caps(caps: &[String]) -> Result<String> {
    Ok(serde_json::to_string(caps)?)
}

type DeviceRaw = (
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    i64,
);

fn map_raw(row: &Row) -> rusqlite::Result<DeviceRaw> {
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
    ))
}

fn from_raw(r: DeviceRaw) -> Result<Device> {
    Ok(Device {
        id: r.0,
        project_id: r.1,
        name: r.2,
        key_prefix: r.3,
        key_hash: r.4,
        // A malformed capability blob decodes to "no advertisement", which the match rule reads as
        // "everything" — the same permissive back-compat answer a pre-M18 row gives, rather than a
        // hard error that would take the whole fleet listing down with one bad row.
        capabilities: r
            .5
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default(),
        last_seen_at: match r.6 {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        agent_version: r.7,
        created_at: parse_ts(&r.8)?,
        revoked: r.9 != 0,
    })
}

/// The `WHERE`-fragment and its bound values that narrow a lease to what a device can run.
///
/// Kept here beside the fleet rather than in the lease statement because it is a *capability*
/// question. A namespace prefix is matched with `substr(…) = '<ns>/'` and not `LIKE '<ns>/%'`:
/// `LIKE` treats `_` as a single-character wildcard, and `_` is legal in an action type, so
/// `ops_x/*` would have silently leased `opsax/…` work to a device that cannot run it.
pub(super) struct CapabilityFilter {
    pub(super) clause: String,
    pub(super) values: Vec<String>,
}

impl CapabilityFilter {
    /// `first_param` is the 1-based index of the first free `?N` slot in the caller's statement.
    pub(super) fn build(capabilities: &[String], first_param: usize) -> CapabilityFilter {
        let unfiltered = CapabilityFilter {
            clause: "1=1".to_string(),
            values: Vec::new(),
        };
        // Empty = a pre-M18 agent or the legacy shared key: no filter, for the back-compat reason
        // on `Store::lease_relay_tasks`.
        if capabilities.is_empty() || capabilities.iter().any(|c| c.trim() == "*") {
            return unfiltered;
        }
        let mut parts = Vec::new();
        let mut values = Vec::new();
        for c in capabilities {
            let c = c.trim();
            if c.is_empty() {
                continue;
            }
            let idx = first_param + values.len();
            match c.strip_suffix("/*") {
                Some(ns) => {
                    let prefix = format!("{ns}/");
                    parts.push(format!(
                        "substr(action_type, 1, {}) = ?{idx}",
                        prefix.chars().count()
                    ));
                    values.push(prefix);
                }
                None => {
                    parts.push(format!("action_type = ?{idx}"));
                    values.push(c.to_string());
                }
            }
        }
        if parts.is_empty() {
            return unfiltered;
        }
        CapabilityFilter {
            clause: format!("({})", parts.join(" OR ")),
            values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lease_filter_binds_every_capability_and_never_interpolates_one() {
        let f = CapabilityFilter::build(&["xprice/*".into(), "ops/nightly".into()], 7);
        assert_eq!(f.values, vec!["xprice/".to_string(), "ops/nightly".into()]);
        assert!(
            f.clause.contains("?7") && f.clause.contains("?8"),
            "{}",
            f.clause
        );
        // The capability text itself never reaches the SQL — it is bound, so a device that
        // advertises `'; DROP TABLE` is a capability nothing matches, not a statement.
        assert!(!f.clause.contains("xprice"), "{}", f.clause);
        // `substr(…) = 'ns/'`, not `LIKE 'ns/%'`: `_` is legal in an action type and is a LIKE
        // wildcard, so `ops_x/*` would otherwise have matched `opsax/…`.
        assert!(
            f.clause.contains("substr(action_type, 1, 7)"),
            "{}",
            f.clause
        );
        assert!(!f.clause.contains("LIKE"), "{}", f.clause);
    }

    #[test]
    fn no_advertisement_and_a_wildcard_both_lease_everything() {
        for caps in [vec![], vec!["*".to_string()], vec![String::new()]] {
            let f = CapabilityFilter::build(&caps, 7);
            assert_eq!(f.clause, "1=1", "caps {caps:?} must not narrow the lease");
            assert!(f.values.is_empty());
        }
    }
}
