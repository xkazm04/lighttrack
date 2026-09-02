//! Per-project alert routing: the `alert_channels` table.
//!
//! `project_id IS NULL` is a **global** channel — the shape the env-configured webhook/ntfy/email
//! destinations have always had. Those are synthesised at startup rather than written here, so
//! adding this table changes nothing for a deployment that has not created a channel.

use rusqlite::{params, Connection, OptionalExtension, Row};

use lighttrack_core::{AlertChannel, AlertKind, ChannelKind, Severity};

use crate::codec::{fmt_ts, parse_ts};
use crate::{Result, StoreError};

const COLS: &str = "id, project_id, kind, target, secret_hash, prev_secret_hash, min_severity, \
    kinds, enabled, created_at";

pub(super) fn create(conn: &Connection, c: &AlertChannel) -> Result<()> {
    conn.execute(
        &format!("INSERT INTO alert_channels ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
        params![
            c.id,
            c.project_id,
            c.kind.as_str(),
            c.target,
            c.secret_hash,
            c.prev_secret_hash,
            c.min_severity.as_str(),
            kinds_json(&c.kinds)?,
            c.enabled as i64,
            fmt_ts(c.created_at),
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<AlertChannel>> {
    let sql = format!("SELECT {COLS} FROM alert_channels WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

/// One set or the other, never both: `Some(p)` is the project's own channels, `None` is the global
/// ones. Unioning them is [`Store::channels_for`](crate::Store::channels_for)'s job, which keeps the
/// "what did I configure for this project" read honest.
pub(super) fn list(conn: &Connection, project: Option<&str>) -> Result<Vec<AlertChannel>> {
    let predicate = match project {
        Some(_) => "project_id = ?1",
        None => "project_id IS NULL",
    };
    let sql = format!("SELECT {COLS} FROM alert_channels WHERE {predicate} ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let raws: rusqlite::Result<Vec<ChannelRaw>> = match project {
        Some(p) => stmt.query_map(params![p], map_raw)?.collect(),
        None => stmt.query_map([], map_raw)?.collect(),
    };
    raws.map_err(StoreError::from)?
        .into_iter()
        .map(from_raw)
        .collect()
}

pub(super) fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM alert_channels WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

fn kinds_json(k: &[AlertKind]) -> Result<Option<String>> {
    if k.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(k)?))
}

type ChannelRaw = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    i64,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<ChannelRaw> {
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

fn from_raw(r: ChannelRaw) -> Result<AlertChannel> {
    Ok(AlertChannel {
        kind: ChannelKind::from_wire(&r.2).ok_or_else(|| {
            StoreError::Other(format!(
                "alert channel '{}' carries an unknown kind '{}'",
                r.0, r.2
            ))
        })?,
        id: r.0,
        project_id: r.1,
        target: r.3,
        secret_hash: r.4,
        prev_secret_hash: r.5,
        min_severity: Severity::from_wire(&r.6),
        // A kind this build does not know is dropped from the *filter*, not from the channel: the
        // alternative is refusing to route anything through a channel a newer release widened.
        kinds: r
            .7
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .map(|v| v.iter().filter_map(|s| AlertKind::from_wire(s)).collect())
            .unwrap_or_default(),
        enabled: r.8 != 0,
        created_at: parse_ts(&r.9)?,
    })
}
