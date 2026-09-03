//! The persisted alert ledger on Postgres.
//!
//! **Why a transaction-scoped advisory lock on the dedup key.** The admission is check-then-act:
//! look back for a live alert with this key, insert if there is none. On the backend that actually
//! runs multi-replica in production, two Cloud Run instances evaluating the same breach in the same
//! second would otherwise both find nothing and both insert — and the operator gets one page per
//! replica, which is precisely the failure the ledger exists to end. `pg_advisory_xact_lock` on
//! `hashtextextended(dedup_key)` serialises only the alerts sharing a key; it is released by the
//! commit or rollback, so a panicking process cannot wedge the gate. (Same mechanism, and the same
//! reasoning, as `admission.rs` — see its module docs for the lock-ordering discipline.)
//!
//! Mirrors `lighttrack-store/src/sqlite/alerts.rs` row for row.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Alert, AlertKind, Delivery, Severity};

use lighttrack_store::codec::decode_event_cursor;
use lighttrack_store::{AlertAdmission, AlertFilter, Result, StoreError};

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

const COLS: &str = "id, project_id, kind, dedup_key, severity, payload, fired_at, delivered, \
    acked_at, acked_by, resolution";

pub(crate) async fn insert_dedup(
    pool: &PgPool,
    a: &Alert,
    cooldown: Duration,
) -> Result<AlertAdmission> {
    let mut tx = pool.begin().await.map_err(pgerr)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(a.dedup_key.clone())
        .execute(&mut *tx)
        .await
        .map_err(pgerr)?;

    if !cooldown.is_zero() {
        let cutoff = a.fired_at
            - chrono::Duration::from_std(cooldown).unwrap_or_else(|_| chrono::Duration::zero());
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT fired_at FROM alerts WHERE dedup_key = $1 AND fired_at > $2 \
             ORDER BY fired_at DESC LIMIT 1",
        )
        .bind(a.dedup_key.clone())
        .bind(fmt_ts(cutoff))
        .fetch_optional(&mut *tx)
        .await
        .map_err(pgerr)?;
        if let Some(ts) = existing {
            tx.commit().await.map_err(pgerr)?;
            return Ok(AlertAdmission::Suppressed {
                fired_at: parse_ts(&ts)?,
            });
        }
    }

    sqlx::query(&format!(
        "INSERT INTO alerts ({COLS}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)"
    ))
    .bind(a.id.clone())
    .bind(a.project_id.clone())
    .bind(a.kind.as_str().to_string())
    .bind(a.dedup_key.clone())
    .bind(a.severity.as_str().to_string())
    .bind(json_or_null(&a.payload)?)
    .bind(fmt_ts(a.fired_at))
    .bind(deliveries_json(&a.delivered)?)
    .bind(a.acked_at.map(fmt_ts))
    .bind(a.acked_by.clone())
    .bind(
        a.resolution
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
    )
    .execute(&mut *tx)
    .await
    .map_err(pgerr)?;
    tx.commit().await.map_err(pgerr)?;
    Ok(AlertAdmission::Admitted)
}

/// Append one delivery outcome. Read-modify-write inside a transaction with `FOR UPDATE`: two
/// channels for the same alert finish concurrently, and a lost update here erases the record that
/// one of them was ever tried.
pub(crate) async fn mark_delivery(pool: &PgPool, alert_id: &str, d: &Delivery) -> Result<bool> {
    let mut tx = pool.begin().await.map_err(pgerr)?;
    let current: Option<Option<String>> =
        sqlx::query_scalar("SELECT delivered FROM alerts WHERE id = $1 FOR UPDATE")
            .bind(alert_id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(pgerr)?;
    let Some(raw) = current else {
        tx.commit().await.map_err(pgerr)?;
        return Ok(false);
    };
    let mut list = parse_deliveries(raw);
    list.push(d.clone());
    sqlx::query("UPDATE alerts SET delivered = $2 WHERE id = $1")
        .bind(alert_id.to_string())
        .bind(deliveries_json(&list)?)
        .execute(&mut *tx)
        .await
        .map_err(pgerr)?;
    tx.commit().await.map_err(pgerr)?;
    Ok(true)
}

pub(crate) async fn get(pool: &PgPool, project: Option<&str>, id: &str) -> Result<Option<Alert>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM alerts WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)"
    ))
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    row.as_ref().map(from_row).transpose()
}

pub(crate) async fn ack(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
    by: &str,
    at: DateTime<Utc>,
) -> Result<bool> {
    let n = sqlx::query("UPDATE alerts SET acked_at = $2, acked_by = $3 WHERE id = $1 AND ($4::text IS NULL OR project_id = $4)")
        .bind(id.to_string())
        .bind(fmt_ts(at))
        .bind(by.to_string())
        .bind(project.map(str::to_string))
        .execute(pool)
        .await
        .map_err(pgerr)?
        .rows_affected();
    Ok(n > 0)
}

pub(crate) async fn attach_resolution(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
    resolution: &Value,
) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE alerts SET resolution = $2 WHERE id = $1 AND ($3::text IS NULL OR project_id = $3)",
    )
    .bind(id.to_string())
    .bind(serde_json::to_string(resolution)?)
    .bind(project.map(str::to_string))
    .execute(pool)
    .await
    .map_err(pgerr)?
    .rows_affected();
    Ok(n > 0)
}

pub(crate) async fn list(pool: &PgPool, f: &AlertFilter) -> Result<Vec<Alert>> {
    // Placeholders are numbered as they are bound, so the WHERE clause is assembled alongside the
    // bindings rather than guessed at afterwards.
    let mut sql = format!("SELECT {COLS} FROM alerts WHERE TRUE");
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = &f.project {
        binds.push(p.clone());
        sql.push_str(&format!(" AND project_id = ${}", binds.len()));
    }
    if let Some(k) = f.kind {
        binds.push(k.as_str().to_string());
        sql.push_str(&format!(" AND kind = ${}", binds.len()));
    }
    if let Some(since) = f.since {
        binds.push(fmt_ts(since));
        sql.push_str(&format!(" AND fired_at >= ${}", binds.len()));
    }
    match f.acked {
        Some(true) => sql.push_str(" AND acked_at IS NOT NULL"),
        Some(false) => sql.push_str(" AND acked_at IS NULL"),
        None => {}
    }
    if let Some((ts, id)) = f.cursor.as_deref().and_then(decode_event_cursor) {
        binds.push(ts.clone());
        let a = binds.len();
        binds.push(ts);
        binds.push(id);
        sql.push_str(&format!(
            " AND (fired_at < ${a} OR (fired_at = ${} AND id < ${}))",
            a + 1,
            a + 2
        ));
    }
    sql.push_str(&format!(
        " ORDER BY fired_at DESC, id DESC LIMIT {}",
        f.effective_limit()
    ));
    let mut q = sqlx::query(&sql);
    for b in binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

fn deliveries_json(d: &[Delivery]) -> Result<Option<String>> {
    if d.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(d)?))
}

fn parse_deliveries(raw: Option<String>) -> Vec<Delivery> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn from_row(r: &PgRow) -> Result<Alert> {
    let id: String = r.try_get(0).map_err(pgerr)?;
    let kind_raw: String = r.try_get(2).map_err(pgerr)?;
    let kind = AlertKind::from_wire(&kind_raw).ok_or_else(|| {
        StoreError::Other(format!("alert '{id}' carries an unknown kind '{kind_raw}'"))
    })?;
    let severity: String = r.try_get(4).map_err(pgerr)?;
    let payload: Option<String> = r.try_get(5).map_err(pgerr)?;
    let fired_at: String = r.try_get(6).map_err(pgerr)?;
    let acked_at: Option<String> = r.try_get(8).map_err(pgerr)?;
    let resolution: Option<String> = r.try_get(10).map_err(pgerr)?;
    Ok(Alert {
        id,
        project_id: r.try_get(1).map_err(pgerr)?,
        kind,
        dedup_key: r.try_get(3).map_err(pgerr)?,
        severity: Severity::from_wire(&severity),
        payload: val_or_null(payload)?,
        fired_at: parse_ts(&fired_at)?,
        delivered: parse_deliveries(r.try_get(7).map_err(pgerr)?),
        acked_at: acked_at.as_deref().map(parse_ts).transpose()?,
        acked_by: r.try_get(9).map_err(pgerr)?,
        resolution: resolution
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    /// The row reader is positional, so `COLS` and `from_row`'s indices are one contract.
    #[test]
    fn cols_line_up_with_the_row_reader() {
        let names = select_list_names(COLS);
        assert_eq!(
            names,
            vec![
                "id",
                "project_id",
                "kind",
                "dedup_key",
                "severity",
                "payload",
                "fired_at",
                "delivered",
                "acked_at",
                "acked_by",
                "resolution",
            ]
        );
    }
}
