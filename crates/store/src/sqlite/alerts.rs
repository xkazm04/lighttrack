//! The persisted alert ledger: dedup-admission, delivery outcomes, ack, resolution.
//!
//! The interesting method is [`insert_alert_dedup`]. Everything else is a row read; that one is the
//! cooldown gate itself, and it has to be a **single critical section** or it decides nothing: two
//! API replicas evaluating the same breach in the same second would each find no recent row, each
//! insert, and each deliver. It runs inside an explicit transaction (the caller already holds the
//! store's write-connection mutex, which is what makes `unchecked_transaction` sound here — the same
//! justification the batch-ingest path uses).

use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use lighttrack_core::{Alert, AlertKind, Delivery, Severity};

use crate::codec::{decode_event_cursor, fmt_ts, json_or_null, parse_ts, val_or_null};
use crate::{AlertAdmission, AlertFilter, Result, StoreError};

const COLS: &str = "id, project_id, kind, dedup_key, severity, payload, fired_at, delivered, \
    acked_at, acked_by, resolution";

/// Admit or suppress, atomically. The predicate is `dedup_key` plus a lower bound on `fired_at` —
/// an index range scan on `idx_alerts_dedup`, so a busy key costs one seek rather than a table walk.
pub(super) fn insert_dedup(
    conn: &Connection,
    a: &Alert,
    cooldown: Duration,
) -> Result<AlertAdmission> {
    let tx = conn.unchecked_transaction()?;
    // A zero cooldown means "never suppress" — the cutoff would be `fired_at` itself, and an alert
    // fired in the same nanosecond as its predecessor is still a separate incident to an operator
    // who asked for no deduplication at all.
    if !cooldown.is_zero() {
        let cutoff =
            a.fired_at - chrono::Duration::from_std(cooldown).unwrap_or(chrono::Duration::zero());
        let existing: Option<String> = tx
            .query_row(
                "SELECT fired_at FROM alerts WHERE dedup_key = ?1 AND fired_at > ?2 \
                 ORDER BY fired_at DESC LIMIT 1",
                params![a.dedup_key, fmt_ts(cutoff)],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(ts) = existing {
            tx.commit()?;
            return Ok(AlertAdmission::Suppressed {
                fired_at: parse_ts(&ts)?,
            });
        }
    }
    tx.execute(
        &format!("INSERT INTO alerts ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"),
        params![
            a.id,
            a.project_id,
            a.kind.as_str(),
            a.dedup_key,
            a.severity.as_str(),
            json_or_null(&a.payload)?,
            fmt_ts(a.fired_at),
            deliveries_json(&a.delivered)?,
            a.acked_at.map(fmt_ts),
            a.acked_by,
            a.resolution
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    tx.commit()?;
    Ok(AlertAdmission::Admitted)
}

/// Append one delivery outcome. Read-modify-write inside a transaction rather than a JSON patch in
/// SQL: two channels for the same alert finish concurrently, and a lost update here would erase the
/// record that one of them was ever tried.
pub(super) fn mark_delivery(conn: &Connection, alert_id: &str, d: &Delivery) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let current: Option<Option<String>> = tx
        .query_row(
            "SELECT delivered FROM alerts WHERE id = ?1",
            params![alert_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = current else {
        tx.commit()?;
        return Ok(false);
    };
    let mut list = parse_deliveries(raw);
    list.push(d.clone());
    tx.execute(
        "UPDATE alerts SET delivered = ?2 WHERE id = ?1",
        params![alert_id, deliveries_json(&list)?],
    )?;
    tx.commit()?;
    Ok(true)
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<Alert>> {
    let sql = format!("SELECT {COLS} FROM alerts WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

pub(super) fn ack(conn: &Connection, id: &str, by: &str, at: DateTime<Utc>) -> Result<bool> {
    let n = conn.execute(
        "UPDATE alerts SET acked_at = ?2, acked_by = ?3 WHERE id = ?1",
        params![id, fmt_ts(at), by],
    )?;
    Ok(n > 0)
}

pub(super) fn attach_resolution(conn: &Connection, id: &str, resolution: &Value) -> Result<bool> {
    let n = conn.execute(
        "UPDATE alerts SET resolution = ?2 WHERE id = ?1",
        params![id, serde_json::to_string(resolution)?],
    )?;
    Ok(n > 0)
}

/// Newest-first, keyset-paged on `(fired_at, id)` — the same cursor codec the event listing uses, so
/// a page boundary that lands inside a burst of same-instant alerts does not skip or repeat one.
pub(super) fn list(conn: &Connection, f: &AlertFilter) -> Result<Vec<Alert>> {
    let mut sql = format!("SELECT {COLS} FROM alerts WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = &f.project {
        args.push(Box::new(p.clone()));
        sql.push_str(&format!(" AND project_id = ?{}", args.len()));
    }
    if let Some(k) = f.kind {
        args.push(Box::new(k.as_str().to_string()));
        sql.push_str(&format!(" AND kind = ?{}", args.len()));
    }
    if let Some(since) = f.since {
        args.push(Box::new(fmt_ts(since)));
        sql.push_str(&format!(" AND fired_at >= ?{}", args.len()));
    }
    match f.acked {
        Some(true) => sql.push_str(" AND acked_at IS NOT NULL"),
        Some(false) => sql.push_str(" AND acked_at IS NULL"),
        None => {}
    }
    if let Some((ts, id)) = f.cursor.as_deref().and_then(decode_event_cursor) {
        args.push(Box::new(ts.clone()));
        let a = args.len();
        args.push(Box::new(ts));
        args.push(Box::new(id));
        sql.push_str(&format!(
            " AND (fired_at < ?{a} OR (fired_at = ?{} AND id < ?{}))",
            a + 1,
            a + 2
        ));
    }
    sql.push_str(&format!(
        " ORDER BY fired_at DESC, id DESC LIMIT {}",
        f.effective_limit()
    ));
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let raws = stmt
        .query_map(refs.as_slice(), map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

fn deliveries_json(d: &[Delivery]) -> Result<Option<String>> {
    if d.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(d)?))
}

/// Deliveries decode leniently: a row whose JSON a newer release widened must still list, because
/// the operator reading this page is trying to find out what happened, not to audit our encoding.
fn parse_deliveries(raw: Option<String>) -> Vec<Delivery> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

type AlertRaw = (
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn map_raw(row: &Row) -> rusqlite::Result<AlertRaw> {
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
    ))
}

pub(super) fn from_raw(r: AlertRaw) -> Result<Alert> {
    let kind = AlertKind::from_wire(&r.2).ok_or_else(|| {
        StoreError::Other(format!("alert '{}' carries an unknown kind '{}'", r.0, r.2))
    })?;
    Ok(Alert {
        id: r.0,
        project_id: r.1,
        // Named rather than guessed: filing an unrecognised kind under an arbitrary known one
        // would make an operator's `?kind=` filter quietly wrong, and dropping the row would hide
        // an alert. Within one release this state is unreachable; across a shared database it is a
        // deployment mistake worth naming.
        kind,
        dedup_key: r.3,
        severity: Severity::from_wire(&r.4),
        payload: val_or_null(r.5)?,
        fired_at: parse_ts(&r.6)?,
        delivered: parse_deliveries(r.7),
        acked_at: r.8.as_deref().map(parse_ts).transpose()?,
        acked_by: r.9,
        resolution: r.10.as_deref().and_then(|s| serde_json::from_str(s).ok()),
    })
}
