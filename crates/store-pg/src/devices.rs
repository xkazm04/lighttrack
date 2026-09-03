//! `Surface::Devices`: the enrolled relay fleet on Postgres (M18).
//!
//! Same semantics as the SQLite reference (`lighttrack-store/src/sqlite/devices.rs`). The
//! eligibility count is a whole-table read decided in Rust for the same reason it is there: the
//! match rule (a namespace prefix stops at a `/`) has to mean one thing in the lease filter, the
//! enqueue verdict and every backend, and a `LIKE` pattern rebuilt per backend is how those quietly
//! drift apart. A fleet is tens of rows, not a workload.

use chrono::Utc;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Device, DeviceEligibility};
use lighttrack_store::Result;

use crate::util::{fmt_ts, parse_ts, pgerr};

pub(crate) const COLS: &str = "id, project_id, name, key_prefix, key_hash, capabilities, \
     last_seen_at, agent_version, created_at, revoked";

pub(crate) async fn create(pool: &PgPool, d: &Device) -> Result<()> {
    sqlx::query(&format!(
        "INSERT INTO devices ({COLS}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)"
    ))
    .bind(d.id.clone())
    .bind(d.project_id.clone())
    .bind(d.name.clone())
    .bind(d.key_prefix.clone())
    .bind(d.key_hash.clone())
    .bind(encode_caps(&d.capabilities)?)
    .bind(d.last_seen_at.map(fmt_ts))
    .bind(d.agent_version.clone())
    .bind(fmt_ts(d.created_at))
    .bind(d.revoked)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

/// One device by id, in `project`'s view of the fleet — the same view [`list`] shows, so the
/// operator-wide devices (`project_id IS NULL`) that serve every project's tasks stay readable.
pub(crate) async fn get(pool: &PgPool, project: Option<&str>, id: &str) -> Result<Option<Device>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM devices \
         WHERE id = $1 AND ($2::text IS NULL OR project_id = $2 OR project_id IS NULL)"
    ))
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

/// Lookup by the presented key's non-secret prefix. A revoked device is returned rather than
/// filtered out, so the caller can refuse it explicitly — "revoked" and "no such device" must stay
/// different answers to whoever is debugging an authentication failure.
pub(crate) async fn find_by_key_prefix(pool: &PgPool, prefix: &str) -> Result<Option<Device>> {
    one(
        pool,
        &format!("SELECT {COLS} FROM devices WHERE key_prefix = $1"),
        prefix,
    )
    .await
}

async fn one(pool: &PgPool, sql: &str, arg: &str) -> Result<Option<Device>> {
    let row = sqlx::query(sql)
        .bind(arg.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

/// One project's devices, or every device when `project` is `None`. A project filter keeps the
/// operator-wide devices (`project_id IS NULL`): they serve every project's tasks, so omitting them
/// would show a project an emptier fleet than the one actually running its work.
pub(crate) async fn list(pool: &PgPool, project: Option<&str>) -> Result<Vec<Device>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM devices \
         WHERE ($1::text IS NULL OR project_id = $1 OR project_id IS NULL) \
         ORDER BY created_at DESC"
    ))
    .bind(project.map(str::to_string))
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn touch(
    pool: &PgPool,
    id: &str,
    capabilities: &[String],
    agent_version: Option<&str>,
) -> Result<()> {
    // An EMPTY report never blanks the row: a pre-M18 agent advertises nothing, and letting that
    // widen the device to "everything" would silently undo the operator's narrowing.
    let caps = if capabilities.is_empty() {
        None
    } else {
        Some(encode_caps(capabilities)?)
    };
    sqlx::query(
        "UPDATE devices SET last_seen_at = $2, \
             capabilities = COALESCE($3, capabilities), \
             agent_version = COALESCE($4, agent_version) \
         WHERE id = $1",
    )
    .bind(id.to_string())
    .bind(fmt_ts(Utc::now()))
    .bind(caps)
    .bind(agent_version.map(str::to_string))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn revoke(pool: &PgPool, project: Option<&str>, id: &str) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE devices SET revoked = TRUE WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)",
    )
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .execute(pool)
    .await
    .map_err(pgerr)?
    .rows_affected();
    Ok(n > 0)
}

pub(crate) async fn count_eligible(pool: &PgPool, action_type: &str) -> Result<DeviceEligibility> {
    let devices = list(pool, None).await?;
    Ok(DeviceEligibility::count(&devices, action_type))
}

fn encode_caps(caps: &[String]) -> Result<String> {
    serde_json::to_string(caps).map_err(|e| {
        lighttrack_store::StoreError::Other(format!("postgres: encoding device capabilities: {e}"))
    })
}

fn from_row(row: &PgRow) -> Result<Device> {
    let caps: Option<String> = row.try_get(5).map_err(pgerr)?;
    let last_seen_at: Option<String> = row.try_get(6).map_err(pgerr)?;
    let created_at: String = row.try_get(8).map_err(pgerr)?;
    Ok(Device {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        name: row.try_get(2).map_err(pgerr)?,
        key_prefix: row.try_get(3).map_err(pgerr)?,
        key_hash: row.try_get(4).map_err(pgerr)?,
        // A malformed blob decodes to "no advertisement", which the match rule reads as
        // "everything" — the permissive back-compat answer, rather than a hard error that would
        // take the whole fleet listing down with one bad row.
        capabilities: caps
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default(),
        last_seen_at: match last_seen_at {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        agent_version: row.try_get(7).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
        revoked: row.try_get(9).map_err(pgerr)?,
    })
}

/// The `WHERE`-fragment and bound values that narrow a lease to what a device can run.
///
/// A namespace prefix is matched with `left(…) = '<ns>/'`, not `LIKE '<ns>/%'`: `LIKE` treats `_`
/// as a single-character wildcard and `_` is legal in an action type, so `ops_x/*` would silently
/// have leased `opsax/…` work to a device that cannot run it.
pub(crate) struct CapabilityFilter {
    pub(crate) clause: String,
    pub(crate) values: Vec<String>,
}

impl CapabilityFilter {
    /// `first_param` is the 1-based index of the first free `$N` slot in the caller's statement.
    pub(crate) fn build(capabilities: &[String], first_param: usize) -> CapabilityFilter {
        let unfiltered = CapabilityFilter {
            clause: "TRUE".to_string(),
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
                        "left(action_type, {}) = ${idx}",
                        prefix.chars().count()
                    ));
                    values.push(prefix);
                }
                None => {
                    parts.push(format!("action_type = ${idx}"));
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
    fn the_column_list_lines_up_with_the_row_reader() {
        // `from_row` reads by POSITION, so a column inserted into COLS without moving the indices
        // would silently read the wrong field into every device.
        let names = crate::util::select_list_names(COLS);
        assert_eq!(names.len(), 10);
        assert_eq!(names[5], "capabilities");
        assert_eq!(names[9], "revoked");
    }

    #[test]
    fn the_lease_filter_binds_every_capability_and_never_interpolates_one() {
        let f = CapabilityFilter::build(&["xprice/*".into(), "ops/nightly".into()], 7);
        assert_eq!(f.values, vec!["xprice/".to_string(), "ops/nightly".into()]);
        assert!(
            f.clause.contains("$7") && f.clause.contains("$8"),
            "{}",
            f.clause
        );
        // The capability text never reaches the SQL — it is bound, so a device advertising
        // `'; DROP TABLE` is a capability nothing matches, not a statement.
        assert!(!f.clause.contains("xprice"), "{}", f.clause);
        // `left(…) = 'ns/'`, not `LIKE 'ns/%'`: `_` is legal in an action type and a LIKE wildcard.
        assert!(f.clause.contains("left(action_type, 7)"), "{}", f.clause);
        assert!(!f.clause.contains("LIKE"), "{}", f.clause);
    }

    #[test]
    fn no_advertisement_and_a_wildcard_both_lease_everything() {
        for caps in [vec![], vec!["*".to_string()], vec![String::new()]] {
            let f = CapabilityFilter::build(&caps, 7);
            assert_eq!(f.clause, "TRUE", "caps {caps:?} must not narrow the lease");
            assert!(f.values.is_empty());
        }
    }
}
