//! `Surface::Contributions` — the contributor-side ledger of what this instance pushed to a hub.
//!
//! Append-only by construction: there is no update and no delete here, because the row *is* the
//! record that something left the building (ARCHITECTURE §12's never-delete rule). The digest body
//! is not stored — only `digest_sha256` and the counts — so the table cannot be mined for more than
//! the hub already knows.
//!
//! Timestamps are the usual fixed-width `RFC3339(Nanos, Z)`, which is what makes the keyset page
//! (`created_at DESC, id DESC`) and the per-hub `latest_contribution` probe correct as plain string
//! comparisons.

use rusqlite::{params, Connection, Row};

use lighttrack_core::{ContributionRecord, ContributionStatus};

use crate::codec::{decode_event_cursor, fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::collective::contributions_limit;
use crate::{Result, StoreError};

const COLS: &str = "id, hub_url_hash, contributor_id_as_acked, schema_version, generated_at, \
     entries_count, projects_included, projects_excluded, digest_sha256, ack, status, created_at";

pub(super) fn insert(conn: &Connection, c: &ContributionRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO collective_contributions \
         (id, hub_url_hash, contributor_id_as_acked, schema_version, generated_at, entries_count, \
          projects_included, projects_excluded, digest_sha256, ack, status, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            c.id,
            c.hub_url_hash,
            c.contributor_id_as_acked,
            c.schema_version as i64,
            fmt_ts(c.generated_at),
            c.entries_count as i64,
            c.projects_included as i64,
            c.projects_excluded as i64,
            c.digest_sha256,
            json_or_null(&c.ack)?,
            c.status.as_str(),
            fmt_ts(c.created_at),
        ],
    )?;
    Ok(())
}

/// Newest-first, keyset-paged. The cursor is the previous page's last `(created_at, id)`, so a row
/// written between two pages cannot shift the window and hide a neighbour — the failure an
/// `OFFSET` page has on an append-only table that is being appended to.
pub(super) fn list(
    conn: &Connection,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Vec<ContributionRecord>> {
    let n = contributions_limit(limit);
    match cursor.map(decode_event_cursor) {
        None => {
            let sql = format!(
                "SELECT {COLS} FROM collective_contributions \
                 ORDER BY created_at DESC, id DESC LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let raws = stmt
                .query_map(params![n as i64], map_raw)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            collect(raws)
        }
        Some(Some((ts, id))) => {
            let sql = format!(
                "SELECT {COLS} FROM collective_contributions \
                 WHERE (created_at < ?1) OR (created_at = ?1 AND id < ?2) \
                 ORDER BY created_at DESC, id DESC LIMIT ?3"
            );
            let mut stmt = conn.prepare(&sql)?;
            let raws = stmt
                .query_map(params![ts, id, n as i64], map_raw)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            collect(raws)
        }
        // A cursor that is not one of ours is a caller error, not an empty page: silently serving
        // page one would look exactly like "the ledger ended here".
        Some(None) => Err(StoreError::Other(
            "bad contributions cursor: not a value this API minted".into(),
        )),
    }
}

/// The hash gate's read: the newest row for one hub. Rides `idx_contributions_hub`.
pub(super) fn latest(conn: &Connection, hub_url_hash: &str) -> Result<Option<ContributionRecord>> {
    let sql = format!(
        "SELECT {COLS} FROM collective_contributions WHERE hub_url_hash = ?1 \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![hub_url_hash], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collect(raws)?.pop())
}

fn collect(rows: Vec<Raw>) -> Result<Vec<ContributionRecord>> {
    rows.into_iter().map(from_raw).collect()
}

struct Raw {
    id: String,
    hub_url_hash: String,
    contributor_id_as_acked: Option<String>,
    schema_version: i64,
    generated_at: String,
    entries_count: i64,
    projects_included: i64,
    projects_excluded: i64,
    digest_sha256: String,
    ack: Option<String>,
    status: String,
    created_at: String,
}

fn map_raw(row: &Row) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: row.get(0)?,
        hub_url_hash: row.get(1)?,
        contributor_id_as_acked: row.get(2)?,
        schema_version: row.get(3)?,
        generated_at: row.get(4)?,
        entries_count: row.get(5)?,
        projects_included: row.get(6)?,
        projects_excluded: row.get(7)?,
        digest_sha256: row.get(8)?,
        ack: row.get(9)?,
        status: row.get(10)?,
        created_at: row.get(11)?,
    })
}

fn from_raw(r: Raw) -> Result<ContributionRecord> {
    // A status outside the vocabulary is surfaced, not coerced: the same argument as `parse_enum`
    // — the reassuring default here would be `Sent`, i.e. "the hub has your data".
    let status = ContributionStatus::from_wire(&r.status).ok_or_else(|| {
        StoreError::Other(format!(
            "stored value {:?} in column `status` is outside the contribution vocabulary",
            r.status
        ))
    })?;
    Ok(ContributionRecord {
        id: r.id,
        hub_url_hash: r.hub_url_hash,
        contributor_id_as_acked: r.contributor_id_as_acked,
        schema_version: r.schema_version as u32,
        generated_at: parse_ts(&r.generated_at)?,
        entries_count: r.entries_count as u32,
        projects_included: r.projects_included as u32,
        projects_excluded: r.projects_excluded as u32,
        digest_sha256: r.digest_sha256,
        ack: val_or_null(r.ack)?,
        status,
        created_at: parse_ts(&r.created_at)?,
    })
}
