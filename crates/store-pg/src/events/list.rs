//! Event reads: the plain listing, the filtered keyset page, and the single-event lookup.

use sqlx::postgres::PgPool;

use lighttrack_core::LlmEvent;
use lighttrack_store::codec::encode_event_cursor;
use lighttrack_store::{EventFilter, EventPage, Result, StoreError};

use super::cols::{from_row, COLS};
use super::filters::list_conds;
use crate::util::{fmt_ts, pgerr};

pub(crate) async fn list(
    pool: &PgPool,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<LlmEvent>> {
    let rows = match project {
        Some(p) => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM events WHERE project_id = $1 ORDER BY ts DESC LIMIT $2"
            ))
            .bind(p.to_string())
            .bind(limit as i64)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(&format!(
                "SELECT {COLS} FROM events ORDER BY ts DESC LIMIT $1"
            ))
            .bind(limit as i64)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

/// Filtered, keyset-paginated listing (newest first), paging on `(ts, id)` descending — the Postgres
/// port of the SQLite reference (`sqlite/events.rs::list_filtered`). Over-fetches by one row to
/// detect a further page; string keyset comparison is chronological thanks to the fixed-width `ts`.
pub(crate) async fn list_filtered(
    pool: &PgPool,
    project: Option<&str>,
    filter: &EventFilter,
    limit: usize,
) -> Result<EventPage> {
    // The extended predicates (status / tag / metadata / min_cost / total count) are not ported to
    // this backend. Answer 501 `unsupported` rather than returning a page that silently ignored the
    // filter — an operator asking "show me the errored calls" must never be handed successful ones.
    if let Some(what) = filter.unsupported_extension() {
        return Err(StoreError::Unsupported(what));
    }
    let conds = list_conds(project, filter)?;
    let where_clause = conds.where_clause();
    // Over-fetch by one so we can tell whether another page exists without a second COUNT query.
    let fetch = (limit as i64).saturating_add(1);
    let sql = format!(
        "SELECT {COLS} FROM events {where_clause}ORDER BY ts DESC, id DESC LIMIT ${}",
        conds.bind_count() + 1
    );
    let mut q = sqlx::query(&sql);
    for b in conds.binds() {
        q = q.bind(b);
    }
    let rows = q.bind(fetch).fetch_all(pool).await.map_err(pgerr)?;
    let mut events = rows
        .iter()
        .map(from_row)
        .collect::<Result<Vec<LlmEvent>>>()?;
    let next_cursor = if events.len() as i64 > limit as i64 {
        events.truncate(limit);
        events
            .last()
            .map(|e| encode_event_cursor(&fmt_ts(e.ts), &e.id))
    } else {
        None
    };
    Ok(EventPage {
        events,
        next_cursor,
        total: None,
    })
}

pub(crate) async fn get(
    pool: &PgPool,
    project: Option<&str>,
    id: &str,
) -> Result<Option<LlmEvent>> {
    let row = sqlx::query(&format!(
        "SELECT {COLS} FROM events WHERE id = $1 AND ($2::text IS NULL OR project_id = $2)"
    ))
    .bind(id.to_string())
    .bind(project.map(str::to_string))
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    match row {
        Some(r) => Ok(Some(from_row(&r)?)),
        None => Ok(None),
    }
}
