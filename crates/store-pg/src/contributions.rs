//! `Surface::Contributions` on Postgres — the contributor-side ledger (M22).
//!
//! Mirrors `lighttrack-store`'s SQLite reference (`sqlite/contributions.rs`): append-only, no
//! update and no delete, timestamps as fixed-width `RFC3339(Nanos, Z)` TEXT so the keyset page
//! (`created_at DESC, id DESC`) is a correct chronological one as a plain string comparison.
//!
//! This is the surface a Neon deployment most needs and would most quietly lack: the hash gate that
//! keeps a scheduled push from tripping a hub's `min_interval` is a *read of this table*, so a
//! backend that defaulted it to empty would send on every interval.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{ContributionRecord, ContributionStatus};
use lighttrack_store::codec::{decode_event_cursor, json_or_null, val_or_null};
use lighttrack_store::collective::contributions_limit;
use lighttrack_store::{Result, StoreError};

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "id, hub_url_hash, contributor_id_as_acked, schema_version, generated_at, \
     entries_count, projects_included, projects_excluded, digest_sha256, ack, status, created_at";

pub(crate) async fn insert(pool: &PgPool, c: &ContributionRecord) -> Result<()> {
    sqlx::query(
        "INSERT INTO collective_contributions \
         (id, hub_url_hash, contributor_id_as_acked, schema_version, generated_at, entries_count, \
          projects_included, projects_excluded, digest_sha256, ack, status, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(&c.id)
    .bind(&c.hub_url_hash)
    .bind(&c.contributor_id_as_acked)
    .bind(c.schema_version as i64)
    .bind(fmt_ts(c.generated_at))
    .bind(c.entries_count as i64)
    .bind(c.projects_included as i64)
    .bind(c.projects_excluded as i64)
    .bind(&c.digest_sha256)
    .bind(json_or_null(&c.ack)?)
    .bind(c.status.as_str())
    .bind(fmt_ts(c.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list(
    pool: &PgPool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Vec<ContributionRecord>> {
    let n = contributions_limit(limit) as i64;
    let rows = match cursor.map(decode_event_cursor) {
        None => {
            let sql = format!(
                "SELECT {COLS} FROM collective_contributions \
                 ORDER BY created_at DESC, id DESC LIMIT $1"
            );
            sqlx::query(&sql).bind(n).fetch_all(pool).await
        }
        Some(Some((ts, id))) => {
            let sql = format!(
                "SELECT {COLS} FROM collective_contributions \
                 WHERE (created_at < $1) OR (created_at = $1 AND id < $2) \
                 ORDER BY created_at DESC, id DESC LIMIT $3"
            );
            sqlx::query(&sql)
                .bind(ts)
                .bind(id)
                .bind(n)
                .fetch_all(pool)
                .await
        }
        // Same refusal as SQLite: serving page one for a cursor we did not mint looks exactly like
        // "the ledger ended here".
        Some(None) => {
            return Err(StoreError::Other(
                "bad contributions cursor: not a value this API minted".into(),
            ))
        }
    };
    rows.map_err(pgerr)?.iter().map(from_row).collect()
}

/// The hash gate's read (`idx_contributions_hub`): the newest row for one hub.
pub(crate) async fn latest(
    pool: &PgPool,
    hub_url_hash: &str,
) -> Result<Option<ContributionRecord>> {
    let sql = format!(
        "SELECT {COLS} FROM collective_contributions WHERE hub_url_hash = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(hub_url_hash)
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

fn from_row(row: &PgRow) -> Result<ContributionRecord> {
    let schema_version: i64 = row.try_get(3).map_err(pgerr)?;
    let entries_count: i64 = row.try_get(5).map_err(pgerr)?;
    let included: i64 = row.try_get(6).map_err(pgerr)?;
    let excluded: i64 = row.try_get(7).map_err(pgerr)?;
    let ack: Option<String> = row.try_get(9).map_err(pgerr)?;
    let status: String = row.try_get(10).map_err(pgerr)?;
    let generated_at: String = row.try_get(4).map_err(pgerr)?;
    let created_at: String = row.try_get(11).map_err(pgerr)?;
    // Surfaced, not coerced — the reassuring default here would be `Sent`, i.e. "the hub has it".
    let status = ContributionStatus::from_wire(&status).ok_or_else(|| {
        StoreError::Other(format!(
            "stored value {status:?} in column `status` is outside the contribution vocabulary"
        ))
    })?;
    Ok(ContributionRecord {
        id: row.try_get(0).map_err(pgerr)?,
        hub_url_hash: row.try_get(1).map_err(pgerr)?,
        contributor_id_as_acked: row.try_get(2).map_err(pgerr)?,
        schema_version: schema_version as u32,
        generated_at: parse_ts(&generated_at)?,
        entries_count: entries_count as u32,
        projects_included: included as u32,
        projects_excluded: excluded as u32,
        digest_sha256: row.try_get(8).map_err(pgerr)?,
        ack: val_or_null(ack)?,
        status,
        created_at: parse_ts(&created_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    /// `from_row` reads by position, so the projection and the reader must agree — they sit far
    /// apart in the file, which is how an off-by-one column read gets introduced.
    #[test]
    fn the_projection_matches_the_positional_reader() {
        let names = select_list_names(COLS);
        assert_eq!(names.len(), 12);
        assert_eq!(names[1], "hub_url_hash");
        assert_eq!(names[4], "generated_at");
        assert_eq!(names[8], "digest_sha256");
        assert_eq!(names[10], "status");
        assert_eq!(names[11], "created_at");
    }
}
