//! Events: ingest, list, single-event lookup, cost rollup, and rolling usage.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::types::ToSql;
use rusqlite::{params, params_from_iter, Connection, ErrorCode, OptionalExtension, Row};
use serde_json::Value;

use lighttrack_core::{
    LimitScope, LlmEvent, Operation, Provider, Status, TokenUsage, TraceSummary,
};

use super::usage_cache::UsageCache;
use crate::codec::{fmt_ts, parse_enum, parse_ts};
use crate::{
    event_contribution, evaluate_admission, Admission, CostRow, EventFilter, EventPage, Result,
    StoreError, TraceFilter, TracePage, Usage, UseCaseCostRow,
};

/// Map a failed event insert to a typed error: a primary-key / uniqueness violation (a duplicate
/// event `id`) becomes [`StoreError::Conflict`] so the API returns 409, not an opaque 500. Anything
/// else keeps its native `Sqlite` mapping.
fn insert_err(e: rusqlite::Error, id: &str) -> StoreError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation => {
            StoreError::Conflict(format!("event '{id}' already exists"))
        }
        _ => e.into(),
    }
}

const COLS: &str = "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, \
    operation, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
    latency_ms, status, error, input, output, tags, source, metadata, name";

pub(super) fn insert(conn: &Connection, ev: &LlmEvent) -> Result<()> {
    let tags = serde_json::to_string(&ev.tags)?;
    let metadata = if ev.metadata.is_null() {
        None
    } else {
        Some(serde_json::to_string(&ev.metadata)?)
    };
    let input = ev.input.as_ref().map(serde_json::to_string).transpose()?;
    let output = ev.output.as_ref().map(serde_json::to_string).transpose()?;
    conn.execute(
        "INSERT INTO events \
         (id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, operation, \
          input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
          latency_ms, status, error, input, output, tags, source, metadata, name) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
        params![
            ev.id,
            ev.project_id,
            ev.trace_id,
            ev.span_id,
            ev.parent_span_id,
            fmt_ts(ev.ts),
            ev.provider.as_str(),
            ev.model,
            ev.operation.as_str(),
            ev.usage.input as i64,
            ev.usage.output as i64,
            ev.usage.cached_input.map(|v| v as i64),
            ev.usage.reasoning.map(|v| v as i64),
            ev.cost_usd,
            ev.latency_ms.map(|v| v as i64),
            ev.status.as_str(),
            ev.error,
            input,
            output,
            tags,
            ev.source,
            metadata,
            ev.name,
        ],
    )
    .map_err(|e| insert_err(e, &ev.id))?;
    Ok(())
}

/// Atomic admission + insert. Because `SqliteStore` runs every call under one locked connection (and
/// holds the `usage_cache` lock alongside it), this whole check-then-act is a single critical
/// section: concurrent ingest is serialized, so a burst cannot all read the same pre-burst usage and
/// race past a cap. The event is inserted only when admitted, so a rejected (over-cap) event is never
/// recorded.
///
/// Rolling usage comes from the incremental [`UsageCache`] — `O(events since the last check)` rather
/// than a full-window re-aggregate — but is byte-for-byte equivalent to the [`usage_since`] /
/// [`usage_since_scoped`] full scans (the property tests in [`super::tests`] pin the equivalence, and
/// those functions remain the reference the cache is checked against).
pub(super) fn insert_checked(
    conn: &Connection,
    cache: &mut UsageCache,
    ev: &LlmEvent,
) -> Result<Admission> {
    let rules = super::limits::list(conn, &ev.project_id, true)?;
    let now = Utc::now();
    let admission = evaluate_admission(&rules, ev, event_contribution(ev), |w, scope| {
        cache.usage(conn, &ev.project_id, w, scope, now)
    })?;
    if admission.admitted {
        insert(conn, ev)?;
    }
    Ok(admission)
}

pub(super) fn list(conn: &Connection, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
    let raws: Vec<RawEvent> = if let Some(p) = project {
        let sql = format!("SELECT {COLS} FROM events WHERE project_id = ?1 ORDER BY ts DESC LIMIT ?2");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![p, limit as i64], map_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    } else {
        let sql = format!("SELECT {COLS} FROM events ORDER BY ts DESC LIMIT ?1");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![limit as i64], map_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    raws.into_iter().map(from_raw).collect()
}

/// Filtered, keyset-paginated listing (newest first), paging on `(ts, id)` descending. Fetches
/// `limit + 1` rows to detect whether a further page exists; when it does, the extra row is dropped and
/// a `next_cursor` encoding the last returned row's `(ts, id)` is returned. String comparison on `ts`
/// is chronological because the stored format is fixed-width (see `codec::fmt_ts`).
pub(super) fn list_filtered(
    conn: &Connection,
    project: Option<&str>,
    filter: &EventFilter,
    limit: usize,
) -> Result<EventPage> {
    let mut conds: Vec<&str> = Vec::new();
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(p) = project {
        conds.push("project_id = ?");
        args.push(Box::new(p.to_string()));
    }
    if let Some(s) = filter.since {
        conds.push("ts >= ?");
        args.push(Box::new(fmt_ts(s)));
    }
    if let Some(u) = filter.until {
        conds.push("ts < ?");
        args.push(Box::new(fmt_ts(u)));
    }
    if let Some(p) = &filter.provider {
        conds.push("provider = ?");
        args.push(Box::new(p.clone()));
    }
    if let Some(m) = &filter.model {
        conds.push("model = ?");
        args.push(Box::new(m.clone()));
    }
    if let Some(t) = &filter.trace_id {
        conds.push("trace_id = ?");
        args.push(Box::new(t.clone()));
    }
    if let Some(n) = &filter.name {
        conds.push("name = ?");
        args.push(Box::new(n.clone()));
    }
    if let Some(cursor) = &filter.cursor {
        let (cts, cid) = decode_cursor(cursor)
            .ok_or_else(|| StoreError::Other(format!("invalid cursor {cursor:?}")))?;
        // Strictly after (cts, cid) in DESC (ts, id) order.
        conds.push("(ts < ? OR (ts = ? AND id < ?))");
        args.push(Box::new(cts.clone()));
        args.push(Box::new(cts));
        args.push(Box::new(cid));
    }

    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conds.join(" AND "))
    };
    // Over-fetch by one so we can tell whether another page exists without a second COUNT query.
    let fetch = (limit as i64).saturating_add(1);
    args.push(Box::new(fetch));
    let sql =
        format!("SELECT {COLS} FROM events {where_clause}ORDER BY ts DESC, id DESC LIMIT ?");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params_from_iter(args.iter()), map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut events =
        raws.into_iter().map(from_raw).collect::<Result<Vec<LlmEvent>>>()?;

    let next_cursor = if events.len() as i64 > limit as i64 {
        events.truncate(limit);
        events
            .last()
            .map(|e| encode_cursor(&fmt_ts(e.ts), &e.id))
    } else {
        None
    };
    Ok(EventPage { events, next_cursor })
}

/// Encode a `(ts, id)` keyset position as an opaque, URL/header-safe cursor (hex of `ts|id`). Both
/// components are `|`-free by construction (fixed-width RFC3339 ts, UUID id), so decoding is exact.
fn encode_cursor(ts: &str, id: &str) -> String {
    let raw = format!("{ts}|{id}");
    raw.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Decode a cursor minted by [`encode_cursor`] back into `(ts, id)`; `None` if it isn't valid hex of a
/// `ts|id` pair.
fn decode_cursor(s: &str) -> Option<(String, String)> {
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    let raw = String::from_utf8(bytes?).ok()?;
    let (ts, id) = raw.split_once('|')?;
    Some((ts.to_string(), id.to_string()))
}

pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<LlmEvent>> {
    let sql = format!("SELECT {COLS} FROM events WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id], map_raw).optional()?;
    raw.map(from_raw).transpose()
}

/// Every event of one trace, oldest first (the order the rollup expects). Skips rows with no
/// `trace_id`. Project-agnostic: a trace id is globally unique, and the caller authorizes the result.
pub(super) fn list_by_trace(conn: &Connection, trace_id: &str) -> Result<Vec<LlmEvent>> {
    let sql = format!("SELECT {COLS} FROM events WHERE trace_id = ?1 ORDER BY ts ASC");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![trace_id], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Per-trace rollups (one row per `trace_id`), most-recent activity first. Aggregated in SQL so
/// listing stays cheap regardless of how many events each trace holds; duration is computed in Rust
/// from the min/max timestamps. Rows without a `trace_id` are excluded.
///
/// Models are *not* aggregated with `GROUP_CONCAT` (whose order is unspecified and drifted from the
/// detail view): [`attach_models`] fetches them in first-seen (min-ts) order in a second query, so
/// `list` and `get_trace` report identical model ordering.
/// Aggregate select-list for a trace summary row. `ended` (MAX ts) is the keyset/order column and
/// the bound `until`/`cursor`/`status`/`min_cost` filters compare against `MAX(ts)`/`SUM(...)`.
const TRACE_SUMMARY_COLS: &str = "trace_id, MIN(project_id) AS project_id, MIN(ts) AS started, \
    MAX(ts) AS ended, COUNT(*) AS spans, COALESCE(SUM(cost_usd),0.0) AS cost, \
    COALESCE(SUM(input_tokens),0) AS it, COALESCE(SUM(output_tokens),0) AS ot, \
    SUM(CASE WHEN status <> 'success' THEN 1 ELSE 0 END) AS errs";

pub(super) fn list_trace_summaries(
    conn: &Connection,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<TraceSummary>> {
    Ok(list_trace_summaries_filtered(conn, project, &TraceFilter::default(), limit)?.traces)
}

/// Filtered, keyset-paginated trace summaries (newest `ended` first), paging on `(ended, trace_id)`
/// descending. `since` is pushed to the event-time `WHERE` so the project+window slice is served by
/// `idx_events_project_ts` (project-scoped) / `idx_events_project_trace` for the grouping rather than
/// scanning the whole table; `until`, `status`, `min_cost`, and the keyset cursor are aggregate-level
/// and so applied in `HAVING`, after grouping. Fetches `limit + 1` rows to detect a further page.
///
/// Note: because `since` prunes at the event level, a trace whose activity straddles `since` rolls up
/// only its in-window spans (its `ended`/set membership stay correct — `ended` is the true MAX ≥
/// `since`). Omitting `since` preserves the full-history rollup exactly.
pub(super) fn list_trace_summaries_filtered(
    conn: &Connection,
    project: Option<&str>,
    filter: &TraceFilter,
    limit: usize,
) -> Result<TracePage> {
    let mut where_conds: Vec<&str> = vec!["trace_id IS NOT NULL", "trace_id <> ''"];
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(p) = project {
        where_conds.push("project_id = ?");
        args.push(Box::new(p.to_string()));
    }
    if let Some(s) = filter.since {
        where_conds.push("ts >= ?");
        args.push(Box::new(fmt_ts(s)));
    }

    // Aggregate-level predicates: the window's upper bound, status, min cost, and the keyset cursor
    // all constrain grouped values, so they belong in HAVING (after GROUP BY), not WHERE.
    let mut having: Vec<&str> = Vec::new();
    if let Some(u) = filter.until {
        having.push("MAX(ts) < ?");
        args.push(Box::new(fmt_ts(u)));
    }
    match filter.status.as_deref() {
        Some("error") => having.push("errs > 0"),
        Some("success") => having.push("errs = 0"),
        _ => {}
    }
    if let Some(mc) = filter.min_cost {
        having.push("cost >= ?");
        args.push(Box::new(mc));
    }
    if let Some(cursor) = &filter.cursor {
        let (cts, cid) = decode_cursor(cursor)
            .ok_or_else(|| StoreError::Other(format!("invalid cursor {cursor:?}")))?;
        // Strictly after (ended, trace_id) in DESC order.
        having.push("(MAX(ts) < ? OR (MAX(ts) = ? AND trace_id < ?))");
        args.push(Box::new(cts.clone()));
        args.push(Box::new(cts));
        args.push(Box::new(cid));
    }

    let where_clause = format!("WHERE {} ", where_conds.join(" AND "));
    let having_clause = if having.is_empty() {
        String::new()
    } else {
        format!("HAVING {} ", having.join(" AND "))
    };
    let fetch = (limit as i64).saturating_add(1);
    args.push(Box::new(fetch));
    let sql = format!(
        "SELECT {TRACE_SUMMARY_COLS} FROM events {where_clause}GROUP BY trace_id \
         {having_clause}ORDER BY ended DESC, trace_id DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params_from_iter(args.iter()), map_trace_summary)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut summaries = raws
        .into_iter()
        .map(trace_summary_from_raw)
        .collect::<Result<Vec<_>>>()?;

    let next_cursor = if summaries.len() as i64 > limit as i64 {
        summaries.truncate(limit);
        summaries
            .last()
            .map(|t| encode_cursor(&fmt_ts(t.ended_at), &t.trace_id))
    } else {
        None
    };
    attach_models(conn, &mut summaries)?;
    Ok(TracePage { traces: summaries, next_cursor })
}

/// Fill each summary's `models` with the trace's distinct models in first-seen (min-ts) order — the
/// same ordering [`lighttrack_core::Trace::from_events`] produces for the detail view. One extra
/// query, scoped to the trace ids actually returned (not N+1). Project-agnostic per trace, matching
/// the detail rollup's `list_by_trace`.
fn attach_models(conn: &Connection, summaries: &mut [TraceSummary]) -> Result<()> {
    if summaries.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?").take(summaries.len()).collect::<Vec<_>>().join(",");
    // Group to one row per (trace, model) with that model's first timestamp, then order globally by
    // that first timestamp; pushing rows in that order builds each trace's list in first-seen order.
    let sql = format!(
        "SELECT trace_id, model FROM \
         (SELECT trace_id, model, MIN(ts) AS mt FROM events WHERE trace_id IN ({placeholders}) \
          GROUP BY trace_id, model) ORDER BY mt ASC"
    );
    let ids: Vec<&str> = summaries.iter().map(|s| s.trace_id.as_str()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(ids.iter()), |row: &Row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_trace: HashMap<String, Vec<String>> = HashMap::new();
    for (trace_id, model) in rows {
        by_trace.entry(trace_id).or_default().push(model);
    }
    for s in summaries.iter_mut() {
        if let Some(models) = by_trace.remove(&s.trace_id) {
            s.models = models;
        }
    }
    Ok(())
}

/// Raw aggregate row for a trace summary, before parsing timestamps. Models are attached separately
/// (see [`attach_models`]).
struct TraceSummaryRaw {
    trace_id: String,
    project_id: String,
    started: String,
    ended: String,
    spans: i64,
    cost_usd: f64,
    input_tokens: i64,
    output_tokens: i64,
    errors: i64,
}

fn map_trace_summary(row: &Row) -> rusqlite::Result<TraceSummaryRaw> {
    Ok(TraceSummaryRaw {
        trace_id: row.get(0)?,
        project_id: row.get(1)?,
        started: row.get(2)?,
        ended: row.get(3)?,
        spans: row.get(4)?,
        cost_usd: row.get(5)?,
        input_tokens: row.get(6)?,
        output_tokens: row.get(7)?,
        errors: row.get(8)?,
    })
}

fn trace_summary_from_raw(r: TraceSummaryRaw) -> Result<TraceSummary> {
    let started_at = parse_ts(&r.started)?;
    let ended_at = parse_ts(&r.ended)?;
    Ok(TraceSummary {
        trace_id: r.trace_id,
        project_id: r.project_id,
        started_at,
        ended_at,
        duration_ms: (ended_at - started_at).num_milliseconds().max(0),
        spans: r.spans as usize,
        cost_usd: r.cost_usd,
        input_tokens: r.input_tokens as u64,
        output_tokens: r.output_tokens as u64,
        total_tokens: (r.input_tokens + r.output_tokens) as u64,
        errors: r.errors as usize,
        status: if r.errors > 0 { "error" } else { "success" }.to_string(),
        models: Vec::new(),
    })
}

pub(super) fn cost_summary(conn: &Connection, project: Option<&str>) -> Result<Vec<CostRow>> {
    let cols = "project_id, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0) AS it, COALESCE(SUM(output_tokens),0) AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost";
    let map = |row: &Row| -> rusqlite::Result<CostRow> {
        Ok(CostRow {
            project_id: row.get(0)?,
            provider: row.get(1)?,
            model: row.get(2)?,
            calls: row.get(3)?,
            input_tokens: row.get(4)?,
            output_tokens: row.get(5)?,
            cost_usd: row.get(6)?,
        })
    };
    let rows = if let Some(p) = project {
        let sql = format!(
            "SELECT {cols} FROM events WHERE project_id = ?1 \
             GROUP BY project_id, provider, model ORDER BY cost DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let v = stmt.query_map(params![p], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
        v
    } else {
        let sql = format!(
            "SELECT {cols} FROM events GROUP BY project_id, provider, model ORDER BY cost DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let v = stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    Ok(rows)
}

/// Cost/usage rollup over an optional `[since, until)` window (both bounds optional). Same grouping /
/// ordering as [`cost_summary`]; window bounds compare against the fixed-width `ts` string.
pub(super) fn cost_summary_windowed(
    conn: &Connection,
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Vec<CostRow>> {
    let cols = "project_id, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0) AS it, COALESCE(SUM(output_tokens),0) AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost";
    let mut conds: Vec<&str> = Vec::new();
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(p) = project {
        conds.push("project_id = ?");
        args.push(Box::new(p.to_string()));
    }
    if let Some(s) = since {
        conds.push("ts >= ?");
        args.push(Box::new(fmt_ts(s)));
    }
    if let Some(u) = until {
        conds.push("ts < ?");
        args.push(Box::new(fmt_ts(u)));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conds.join(" AND "))
    };
    let sql = format!(
        "SELECT {cols} FROM events {where_clause}GROUP BY project_id, provider, model ORDER BY cost DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), |row: &Row| {
            Ok(CostRow {
                project_id: row.get(0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                calls: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cost_usd: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn usage_since(conn: &Connection, project: &str, since: DateTime<Utc>) -> Result<Usage> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(SUM(cost_usd),0.0), COUNT(*), \
         COALESCE(SUM(input_tokens + output_tokens),0) \
         FROM events WHERE project_id = ?1 AND ts >= ?2",
    )?;
    let usage = stmt.query_row(params![project, fmt_ts(since)], |row| {
        Ok(Usage {
            cost_usd: row.get(0)?,
            calls: row.get(1)?,
            tokens: row.get(2)?,
        })
    })?;
    Ok(usage)
}

/// Rolling usage for one project since `since`, restricted to a single scope dimension. The scoped
/// column (`provider` / `model` / `name`) is chosen by the [`LimitScope`] variant; the `name` case
/// matches only rows whose use-case equals the value (a NULL `name` never matches a name scope). The
/// `(project_id, ts)` and `(project_id, name, ts)` indexes cover the project+window filter.
pub(super) fn usage_since_scoped(
    conn: &Connection,
    project: &str,
    since: DateTime<Utc>,
    scope: &LimitScope,
) -> Result<Usage> {
    // `column` is a fixed keyword from the enum (never user input) — safe to interpolate.
    let column = match scope {
        LimitScope::Provider(_) => "provider",
        LimitScope::Model(_) => "model",
        LimitScope::Name(_) => "name",
    };
    let sql = format!(
        "SELECT COALESCE(SUM(cost_usd),0.0), COUNT(*), \
         COALESCE(SUM(input_tokens + output_tokens),0) \
         FROM events WHERE project_id = ?1 AND ts >= ?2 AND {column} = ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let usage = stmt.query_row(params![project, fmt_ts(since), scope.value()], |row| {
        Ok(Usage {
            cost_usd: row.get(0)?,
            calls: row.get(1)?,
            tokens: row.get(2)?,
        })
    })?;
    Ok(usage)
}

/// Raw column values as stored, before reconstructing an `LlmEvent`.
struct RawEvent {
    id: String,
    project_id: String,
    trace_id: Option<String>,
    span_id: Option<String>,
    parent_span_id: Option<String>,
    ts: String,
    provider: String,
    model: String,
    operation: String,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cost_usd: Option<f64>,
    latency_ms: Option<i64>,
    status: String,
    error: Option<String>,
    input: Option<String>,
    output: Option<String>,
    tags: Option<String>,
    source: Option<String>,
    metadata: Option<String>,
    name: Option<String>,
}

fn map_raw(row: &Row) -> rusqlite::Result<RawEvent> {
    Ok(RawEvent {
        id: row.get(0)?,
        project_id: row.get(1)?,
        trace_id: row.get(2)?,
        span_id: row.get(3)?,
        parent_span_id: row.get(4)?,
        ts: row.get(5)?,
        provider: row.get(6)?,
        model: row.get(7)?,
        operation: row.get(8)?,
        input_tokens: row.get(9)?,
        output_tokens: row.get(10)?,
        cached_input_tokens: row.get(11)?,
        reasoning_tokens: row.get(12)?,
        cost_usd: row.get(13)?,
        latency_ms: row.get(14)?,
        status: row.get(15)?,
        error: row.get(16)?,
        input: row.get(17)?,
        output: row.get(18)?,
        tags: row.get(19)?,
        source: row.get(20)?,
        metadata: row.get(21)?,
        name: row.get(22)?,
    })
}

fn from_raw(r: RawEvent) -> Result<LlmEvent> {
    let ts = parse_ts(&r.ts)?;
    let input = match r.input {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    };
    let output = match r.output {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    };
    let tags: Vec<String> = match r.tags {
        Some(s) => serde_json::from_str(&s)?,
        None => Vec::new(),
    };
    let metadata: Value = match r.metadata {
        Some(s) => serde_json::from_str(&s)?,
        None => Value::Null,
    };
    Ok(LlmEvent {
        id: r.id,
        project_id: r.project_id,
        trace_id: r.trace_id,
        span_id: r.span_id,
        parent_span_id: r.parent_span_id,
        ts,
        provider: parse_enum::<Provider>(&r.provider),
        model: r.model,
        name: r.name,
        operation: parse_enum::<Operation>(&r.operation),
        usage: TokenUsage {
            input: r.input_tokens as u64,
            output: r.output_tokens as u64,
            cached_input: r.cached_input_tokens.map(|v| v as u64),
            reasoning: r.reasoning_tokens.map(|v| v as u64),
        },
        cost_usd: r.cost_usd,
        latency_ms: r.latency_ms.map(|v| v as u64),
        status: parse_enum::<Status>(&r.status),
        error: r.error,
        input,
        output,
        tags,
        source: r.source,
        metadata,
    })
}

/// Use-case rollup: group usage + cost by (name, provider, model), optionally restricted to events
/// at/after `since` (the rolling-window start). Un-named calls (`name IS NULL`) group together per
/// model, so the consumer can fold them under their model. Ordered by cost, most expensive first.
pub(super) fn usecase_costs(
    conn: &Connection,
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<UseCaseCostRow>> {
    let cols = "name, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0) AS it, COALESCE(SUM(output_tokens),0) AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost";
    let tail = "GROUP BY name, provider, model ORDER BY cost DESC";
    let map = |row: &Row| -> rusqlite::Result<UseCaseCostRow> {
        Ok(UseCaseCostRow {
            name: row.get(0)?,
            provider: row.get(1)?,
            model: row.get(2)?,
            calls: row.get(3)?,
            input_tokens: row.get(4)?,
            output_tokens: row.get(5)?,
            cost_usd: row.get(6)?,
        })
    };
    let since_str = since.map(fmt_ts);
    // Bind the collected Vec to `v` and return it (not the query_map tail expression directly) so
    // `stmt` outlives the borrow — mirrors `cost_summary` above.
    let rows = match (project, since_str.as_deref()) {
        (Some(p), Some(s)) => {
            let sql = format!("SELECT {cols} FROM events WHERE project_id = ?1 AND ts >= ?2 {tail}");
            let mut stmt = conn.prepare(&sql)?;
            let v = stmt.query_map(params![p, s], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        }
        (Some(p), None) => {
            let sql = format!("SELECT {cols} FROM events WHERE project_id = ?1 {tail}");
            let mut stmt = conn.prepare(&sql)?;
            let v = stmt.query_map(params![p], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        }
        (None, Some(s)) => {
            let sql = format!("SELECT {cols} FROM events WHERE ts >= ?1 {tail}");
            let mut stmt = conn.prepare(&sql)?;
            let v = stmt.query_map(params![s], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        }
        (None, None) => {
            let sql = format!("SELECT {cols} FROM events {tail}");
            let mut stmt = conn.prepare(&sql)?;
            let v = stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        }
    };
    Ok(rows)
}
