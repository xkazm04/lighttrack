//! The event select lists and the readers that consume them.
//!
//! `COLS` and [`from_row`] are **one contract**: the reader addresses columns by position, so the
//! list and the `try_get` indices only make sense together and therefore live in one file. The same
//! holds for [`USAGE_COLS`] and [`map_usage`].

use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::Row;

use lighttrack_core::{LlmEvent, Operation, Provider, Status, TokenUsage};
use lighttrack_store::{Result, Usage};

use crate::util::{parse_enum, parse_ts, pgerr};

pub(crate) const COLS: &str =
    "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, \
    operation, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
    latency_ms, status, error, input, output, tags, source, metadata, name, \
    COALESCE(received_at, ts) AS received_at";

/// Windowed accounting keys on **`received_at`** (server arrival), never the client's `ts`, so a
/// skewed or deliberately backdated clock cannot slide spend outside the window a cap is evaluated
/// over. `COALESCE` keeps rows written before the column existed (backfilled by the schema, but the
/// coalesce also covers a row inserted by an older binary mid-deploy) counted at their `ts`.
pub(crate) const RECEIVED: &str = "COALESCE(received_at, ts)";

/// Rolling-usage aggregates, mirroring the SQLite reference (`sqlite/events.rs::USAGE_COLS`). Beyond
/// the three totals it reports cost *provenance*: how many calls carried no price at all (the price
/// book had no entry — we never write a phantom zero onto the row) and how much of the sum the client
/// self-reported. The limit path charges unpriced calls by imputation instead of letting them cost
/// `$0.00`, and surfaces the client-reported share.
pub(crate) const USAGE_COLS: &str = "COALESCE(SUM(cost_usd),0.0), COUNT(*), \
     COALESCE(SUM(input_tokens + output_tokens),0)::bigint, \
     COUNT(*) FILTER (WHERE cost_usd IS NULL)::bigint, \
     COALESCE(SUM(cost_usd) FILTER (WHERE (NULLIF(metadata,'')::jsonb)->>'cost_source' = 'client'),0.0)";
// NOTE on the cast: `metadata` is TEXT and `::jsonb` RAISES on invalid JSON — and this query is on
// the admission path, so one bad row would stop ingest for the whole project, not just skew a
// provenance number. Every row we write is serde-serialized JSON or NULL, and `NULLIF` covers the
// empty string (the one malformed value a hand-edited or legacy row realistically carries). It is
// not a total guarantee: arbitrary non-JSON text in `metadata` would still raise. If that ever
// becomes reachable, move the provenance sum out of the admission query rather than widening the
// cast — enforcement must never depend on parsing a free-form column.

pub(crate) fn map_usage(row: &PgRow) -> Result<Usage> {
    Ok(Usage {
        cost_usd: row.try_get(0).map_err(pgerr)?,
        calls: row.try_get(1).map_err(pgerr)?,
        tokens: row.try_get(2).map_err(pgerr)?,
        unpriced_calls: row.try_get(3).map_err(pgerr)?,
        client_cost_usd: row.try_get(4).map_err(pgerr)?,
    })
}

pub(crate) fn from_row(row: &PgRow) -> Result<LlmEvent> {
    let ts: String = row.try_get(5).map_err(pgerr)?;
    let provider: String = row.try_get(6).map_err(pgerr)?;
    let operation: String = row.try_get(8).map_err(pgerr)?;
    let status: String = row.try_get(15).map_err(pgerr)?;
    let input: Option<String> = row.try_get(17).map_err(pgerr)?;
    let output: Option<String> = row.try_get(18).map_err(pgerr)?;
    let tags: Option<String> = row.try_get(19).map_err(pgerr)?;
    let metadata: Option<String> = row.try_get(21).map_err(pgerr)?;
    let received_at: String = row.try_get(23).map_err(pgerr)?;

    Ok(LlmEvent {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        trace_id: row.try_get(2).map_err(pgerr)?,
        span_id: row.try_get(3).map_err(pgerr)?,
        parent_span_id: row.try_get(4).map_err(pgerr)?,
        ts: parse_ts(&ts)?,
        received_at: parse_ts(&received_at)?,
        provider: parse_enum::<Provider>("provider", &provider)?,
        model: row.try_get(7).map_err(pgerr)?,
        name: row.try_get(22).map_err(pgerr)?,
        operation: parse_enum::<Operation>("operation", &operation)?,
        usage: TokenUsage {
            input: row.try_get::<i64, _>(9).map_err(pgerr)? as u64,
            output: row.try_get::<i64, _>(10).map_err(pgerr)? as u64,
            cached_input: row
                .try_get::<Option<i64>, _>(11)
                .map_err(pgerr)?
                .map(|v| v as u64),
            reasoning: row
                .try_get::<Option<i64>, _>(12)
                .map_err(pgerr)?
                .map(|v| v as u64),
        },
        cost_usd: row.try_get(13).map_err(pgerr)?,
        latency_ms: row
            .try_get::<Option<i64>, _>(14)
            .map_err(pgerr)?
            .map(|v| v as u64),
        status: parse_enum::<Status>("status", &status)?,
        error: row.try_get(16).map_err(pgerr)?,
        input: match input {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        },
        output: match output {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        },
        tags: match tags {
            Some(s) => serde_json::from_str(&s)?,
            None => Vec::new(),
        },
        source: row.try_get(20).map_err(pgerr)?,
        metadata: match metadata {
            Some(s) => serde_json::from_str(&s)?,
            None => Value::Null,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::select_list_names;

    /// `from_row` reads by position, so `COLS` and the `try_get` indices are one contract. Adding a
    /// column mid-list without moving the reads shifts every field after it — a silent corruption
    /// no type error would catch, since most of these are strings.
    #[test]
    fn cols_match_the_positions_from_row_reads() {
        let names = select_list_names(COLS);
        assert_eq!(
            names.len(),
            24,
            "COLS has {} entries: {names:?}",
            names.len()
        );
        for (i, expected) in [
            (0, "id"),
            (5, "ts"),
            (6, "provider"),
            (8, "operation"),
            (9, "input_tokens"),
            (10, "output_tokens"),
            (13, "cost_usd"),
            (14, "latency_ms"),
            (15, "status"),
            (17, "input"),
            (18, "output"),
            (19, "tags"),
            (20, "source"),
            (21, "metadata"),
            (22, "name"),
            (23, "received_at"),
        ] {
            assert_eq!(names[i], expected, "column {i} moved");
        }
    }

    /// The window expression must prefer server arrival and only fall back to `ts` — "simplifying"
    /// it to plain `ts` hands a client with a backdated clock unmetered traffic. The behavior itself
    /// is pinned by `tests/received_at.rs`, which needs a live Postgres; this keeps the expression
    /// from being edited away without one.
    #[test]
    fn window_expression_prefers_received_at_over_ts() {
        assert_eq!(RECEIVED, "COALESCE(received_at, ts)");
    }
}
