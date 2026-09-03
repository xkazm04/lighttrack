//! Per-project alert routing on Postgres. Mirrors `lighttrack-store/src/sqlite/alert_channels.rs`;
//! `enabled` is a real `BOOLEAN` here rather than SQLite's integer, which is the only difference the
//! row mapper has to know about.

use chrono::Utc;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{AlertChannel, AlertKind, ChannelKind, Severity};
use lighttrack_store::{Result, StoreError};

use crate::util::{fmt_ts, parse_ts, pgerr};

const COLS: &str = "id, project_id, kind, target, secret_hash, prev_secret_hash, min_severity, \
    kinds, enabled, created_at";

pub(crate) async fn create(pool: &PgPool, c: &AlertChannel) -> Result<()> {
    sqlx::query(&format!(
        "INSERT INTO alert_channels ({COLS}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)"
    ))
    .bind(c.id.clone())
    .bind(c.project_id.clone())
    .bind(c.kind.as_str().to_string())
    .bind(c.target.clone())
    .bind(c.secret_hash.clone())
    .bind(c.prev_secret_hash.clone())
    .bind(c.min_severity.as_str().to_string())
    .bind(kinds_json(&c.kinds)?)
    .bind(c.enabled)
    .bind(fmt_ts(c.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

/// One channel by id, in `project`'s scope. An operator scope (`None`) reads the project-less
/// channels it owns, mirroring [`list`] — the two reads must agree on what a scope can see.
pub(crate) async fn get(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
) -> Result<Option<AlertChannel>> {
    let predicate = match project {
        Some(_) => "project_id = $2",
        None => "project_id IS NULL",
    };
    let sql = format!("SELECT {COLS} FROM alert_channels WHERE id = $1 AND {predicate}");
    let q = sqlx::query(&sql).bind(id.to_string());
    let q = match project {
        Some(p) => q.bind(p.to_string()),
        None => q,
    };
    let row = q.fetch_optional(pool).await.map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

/// One set or the other: `Some(p)` is the project's own channels, `None` the global ones. The union
/// is [`Store::channels_for`](lighttrack_store::Store::channels_for)'s job.
pub(crate) async fn list(pool: &PgPool, project: Option<&str>) -> Result<Vec<AlertChannel>> {
    let rows = match project {
        Some(p) => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM alert_channels WHERE project_id = $1 ORDER BY created_at"
            ))
            .bind(p.to_string())
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM alert_channels WHERE project_id IS NULL ORDER BY created_at"
            ))
            .fetch_all(pool)
            .await
        }
    }
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn delete(pool: &PgPool, project: Option<&str>, id: &str) -> Result<bool> {
    let sql = match project {
        Some(_) => "DELETE FROM alert_channels WHERE id = $1 AND project_id = $2",
        None => "DELETE FROM alert_channels WHERE id = $1 AND project_id IS NULL",
    };
    let q = sqlx::query(sql).bind(id.to_string());
    let q = match project {
        Some(p) => q.bind(p.to_string()),
        None => q,
    };
    let n = q.execute(pool).await.map_err(pgerr)?.rows_affected();
    Ok(n > 0)
}

fn kinds_json(k: &[AlertKind]) -> Result<Option<String>> {
    if k.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(k)?))
}

fn from_row(r: &PgRow) -> Result<AlertChannel> {
    let id: String = r.try_get(0).map_err(pgerr)?;
    let kind_raw: String = r.try_get(2).map_err(pgerr)?;
    let kind = ChannelKind::from_wire(&kind_raw).ok_or_else(|| {
        StoreError::Other(format!(
            "alert channel '{id}' carries an unknown kind '{kind_raw}'"
        ))
    })?;
    let min_severity: String = r.try_get(6).map_err(pgerr)?;
    let kinds: Option<String> = r.try_get(7).map_err(pgerr)?;
    let created_at: Option<String> = r.try_get(9).map_err(pgerr)?;
    Ok(AlertChannel {
        id,
        project_id: r.try_get(1).map_err(pgerr)?,
        kind,
        target: r.try_get(3).map_err(pgerr)?,
        secret_hash: r.try_get(4).map_err(pgerr)?,
        prev_secret_hash: r.try_get(5).map_err(pgerr)?,
        min_severity: Severity::from_wire(&min_severity),
        // A kind this build does not know is dropped from the *filter*, not from the channel: the
        // alternative is refusing to route anything through a channel a newer release widened.
        kinds: kinds
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .map(|v| v.iter().filter_map(|s| AlertKind::from_wire(s)).collect())
            .unwrap_or_default(),
        enabled: r.try_get(8).map_err(pgerr)?,
        created_at: created_at
            .as_deref()
            .map(parse_ts)
            .transpose()?
            .unwrap_or_else(Utc::now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    #[test]
    fn cols_line_up_with_the_row_reader() {
        assert_eq!(
            select_list_names(COLS),
            vec![
                "id",
                "project_id",
                "kind",
                "target",
                "secret_hash",
                "prev_secret_hash",
                "min_severity",
                "kinds",
                "enabled",
                "created_at",
            ]
        );
    }
}
