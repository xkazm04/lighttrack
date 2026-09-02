//! Event ingest: the INSERT statement (shared with the admission transaction) and its error mapping.

use sqlx::postgres::PgPool;

use lighttrack_core::LlmEvent;
use lighttrack_store::{Result, StoreError};

use crate::util::{fmt_ts, pgerr};

/// Map a failed event insert to a typed error: SQLSTATE 23505 (unique violation — a duplicate
/// event `id`) becomes [`StoreError::Conflict`] so the API returns 409, not an opaque 500.
/// Mirrors the SQLite backend's `insert_err`.
pub(crate) fn insert_err(e: sqlx::Error, id: &str) -> StoreError {
    if let sqlx::Error::Database(db) = &e {
        if db.code().as_deref() == Some("23505") {
            return StoreError::Conflict(format!("event '{id}' already exists"));
        }
    }
    pgerr(e)
}

pub(crate) async fn insert(pool: &PgPool, ev: &LlmEvent) -> Result<()> {
    insert_query(ev)?
        .execute(pool)
        .await
        .map_err(|e| insert_err(e, &ev.id))?;
    Ok(())
}

/// The event INSERT as a *value*, so the same statement (and the same column list) serves both the
/// pooled write above and the admission transaction in [`crate::admission`] — the alternative,
/// a second hand-maintained INSERT, is how a column ends up written on one path and not the other.
/// Every bind is owned, hence `'static`.
pub(crate) fn insert_query(
    ev: &LlmEvent,
) -> Result<sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments>> {
    let tags = serde_json::to_string(&ev.tags)?;
    let metadata = if ev.metadata.is_null() {
        None
    } else {
        Some(serde_json::to_string(&ev.metadata)?)
    };
    let input = match &ev.input {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };
    let output = match &ev.output {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };
    Ok(sqlx::query(
        "INSERT INTO events (id, project_id, trace_id, span_id, parent_span_id, ts, \
         provider, model, operation, input_tokens, output_tokens, cached_input_tokens, \
         reasoning_tokens, cost_usd, latency_ms, status, error, input, output, tags, \
         source, metadata, name, received_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24)",
    )
    .bind(ev.id.clone())
    .bind(ev.project_id.clone())
    .bind(ev.trace_id.clone())
    .bind(ev.span_id.clone())
    .bind(ev.parent_span_id.clone())
    .bind(fmt_ts(ev.ts))
    .bind(ev.provider.as_str().to_string()) // owned: the id is no longer a 'static enum label
    .bind(ev.model.clone())
    .bind(ev.operation.as_str())
    .bind(ev.usage.input as i64)
    .bind(ev.usage.output as i64)
    .bind(ev.usage.cached_input.map(|v| v as i64))
    .bind(ev.usage.reasoning.map(|v| v as i64))
    .bind(ev.cost_usd)
    .bind(ev.latency_ms.map(|v| v as i64))
    .bind(ev.status.as_str())
    .bind(ev.error.clone())
    .bind(input)
    .bind(output)
    .bind(tags)
    .bind(ev.source.clone())
    .bind(metadata)
    .bind(ev.name.clone())
    .bind(fmt_ts(ev.received_at)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `sqlx::Error::Database` with a chosen SQLSTATE — the driver's own error type has no public
    /// constructor, and the 23505 → 409 mapping is worth pinning without a live server.
    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake db error {}", self.0)
        }
    }
    impl std::error::Error for FakeDbError {}
    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "fake db error"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn db_err(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code)))
    }

    /// A duplicate event id is the client re-sending, not a server fault: it must surface as 409,
    /// never as an opaque 500.
    #[test]
    fn duplicate_id_maps_to_conflict() {
        match insert_err(db_err("23505"), "ev-1") {
            StoreError::Conflict(msg) => assert!(msg.contains("ev-1"), "{msg}"),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn other_database_errors_stay_generic() {
        // A different SQLSTATE (23503 = foreign key violation) is not a duplicate.
        assert!(matches!(
            insert_err(db_err("23503"), "ev-1"),
            StoreError::Other(_)
        ));
        // Nor is a non-database error.
        assert!(matches!(
            insert_err(sqlx::Error::RowNotFound, "ev-1"),
            StoreError::Other(_)
        ));
    }
}
