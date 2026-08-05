//! Traces on Postgres: roll the events sharing a `trace_id` into a listing, a detail window, and the
//! verdicts attached to them.
//!
//! Port of the SQLite reference (`store/src/sqlite/events.rs::list_trace_summaries_filtered` /
//! `list_by_trace` and `sqlite/scores.rs::list_by_trace`) — the semantics, not just the shape:
//! grouping, the `(ended, trace_id)` DESC keyset, first-seen model ordering in one extra query
//! (never N+1), the span cap with its true-total truncation signal, and the `TraceShape` duration
//! rule the list and the detail view must both report.
//!
//! Every read is **scoped by project in the query**. A `trace_id` is caller-supplied and therefore
//! not a tenant boundary: two projects can pick `"req-1"`, and a colliding id elsewhere must be
//! invisible here rather than merged in and then authorized away. `None` reads across projects and
//! is reserved for operator principals.

use std::collections::HashMap;

use chrono::{TimeZone, Utc};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Score, TraceShape, TraceSummary};
use lighttrack_store::codec::{decode_event_cursor, encode_event_cursor};
use lighttrack_store::{Result, StoreError, TraceEvents, TraceFilter, TracePage};

use crate::events::{from_row as event_from_row, COLS as EVENT_COLS};
use crate::scores::{from_row as score_from_row, COLS as SCORE_COLS};
use crate::util::{fmt_ts, parse_ts, pgerr};

/// One bound value. The trace queries mix text, a cost threshold, and the row limit, so the binds
/// can't be the `Vec<String>` the other modules get away with.
enum Arg {
    S(String),
    F(f64),
    I(i64),
}

type Query<'a> = sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments>;

fn bind_all<'a>(mut q: Query<'a>, args: &'a [Arg]) -> Query<'a> {
    for a in args {
        q = match a {
            Arg::S(s) => q.bind(s),
            Arg::F(f) => q.bind(*f),
            Arg::I(i) => q.bind(*i),
        };
    }
    q
}

/// Aggregate select-list for a trace summary row, mirroring SQLite's `TRACE_SUMMARY_COLS`.
///
/// `finish_ms` is the aggregate that lets the list report the *same* duration as the detail view:
/// `max(ts + latency)` in epoch milliseconds, not `MAX(ts)`. Like the SQLite expression it leans on
/// the fixed-width `RFC3339(Nanos, Z)` invariant — whole seconds from the truncated timestamp,
/// characters 21..24 are the milliseconds — and both endpoints then go through [`TraceShape`], the
/// one definition. `date_trunc` (rather than flooring a fractional epoch) keeps the seconds exact on
/// every server version instead of depending on `EXTRACT`'s numeric-vs-float return type.
///
/// `SUM` over a `BIGINT` column is `numeric` in Postgres, hence the `::bigint` casts.
const SUMMARY_COLS: &str = "trace_id, MIN(project_id) AS project_id, MIN(ts) AS started, \
    MAX(ts) AS ended, \
    MAX(EXTRACT(EPOCH FROM date_trunc('second', ts::timestamptz))::bigint * 1000 \
        + COALESCE(NULLIF(substr(ts, 21, 3), '')::bigint, 0) \
        + COALESCE(latency_ms, 0)) AS finish_ms, \
    COUNT(*)::bigint AS spans, COALESCE(SUM(cost_usd),0.0) AS cost, \
    COALESCE(SUM(input_tokens),0)::bigint AS it, COALESCE(SUM(output_tokens),0)::bigint AS ot, \
    COUNT(*) FILTER (WHERE status <> 'success')::bigint AS errs";

/// Unfiltered, unpaginated listing — the plain "most recent traces" read.
pub(crate) async fn list_summaries(
    pool: &PgPool,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<TraceSummary>> {
    Ok(
        list_summaries_filtered(pool, project, &TraceFilter::default(), limit)
            .await?
            .traces,
    )
}

/// Filtered, keyset-paginated trace summaries (newest `ended` first), paging on `(ended, trace_id)`
/// descending. `since` is pushed to the event-level `WHERE` so the project+window slice is served by
/// `idx_events_project_ts`; `until`, `status`, `min_cost`, and the cursor constrain *grouped* values
/// and so live in `HAVING`. Postgres can't reference select-list aliases in `HAVING`, so those
/// predicates repeat the aggregate expression. Fetches `limit + 1` rows to detect a further page.
///
/// As on SQLite: because `since` prunes at the event level, a trace straddling it rolls up only its
/// in-window spans (its `ended` and set membership stay correct).
pub(crate) async fn list_summaries_filtered(
    pool: &PgPool,
    project: Option<&str>,
    filter: &TraceFilter,
    limit: usize,
) -> Result<TracePage> {
    let mut args: Vec<Arg> = Vec::new();
    let mut conds: Vec<String> = vec!["trace_id IS NOT NULL".into(), "trace_id <> ''".into()];
    if let Some(p) = project {
        args.push(Arg::S(p.to_string()));
        conds.push(format!("project_id = ${}", args.len()));
    }
    if let Some(s) = filter.since {
        args.push(Arg::S(fmt_ts(s)));
        conds.push(format!("ts >= ${}", args.len()));
    }

    let mut having: Vec<String> = Vec::new();
    if let Some(u) = filter.until {
        args.push(Arg::S(fmt_ts(u)));
        having.push(format!("MAX(ts) < ${}", args.len()));
    }
    match filter.status.as_deref() {
        Some("error") => having.push("COUNT(*) FILTER (WHERE status <> 'success') > 0".into()),
        Some("success") => having.push("COUNT(*) FILTER (WHERE status <> 'success') = 0".into()),
        _ => {}
    }
    if let Some(mc) = filter.min_cost {
        args.push(Arg::F(mc));
        having.push(format!("COALESCE(SUM(cost_usd),0.0) >= ${}", args.len()));
    }
    if let Some(cursor) = &filter.cursor {
        let (cts, cid) = decode_event_cursor(cursor)
            .ok_or_else(|| StoreError::Other(format!("invalid cursor {cursor:?}")))?;
        args.push(Arg::S(cts));
        let i = args.len();
        args.push(Arg::S(cid));
        let j = args.len();
        // Strictly after (ended, trace_id) in DESC order.
        having.push(format!(
            "(MAX(ts) < ${i} OR (MAX(ts) = ${i} AND trace_id < ${j}))"
        ));
    }

    // Over-fetch by one so a further page is detected without a second COUNT.
    args.push(Arg::I((limit as i64).saturating_add(1)));
    let limit_ph = args.len();
    let having_clause = if having.is_empty() {
        String::new()
    } else {
        format!("HAVING {} ", having.join(" AND "))
    };
    let sql = format!(
        "SELECT {SUMMARY_COLS} FROM events WHERE {} GROUP BY trace_id {having_clause}\
         ORDER BY ended DESC, trace_id DESC LIMIT ${limit_ph}",
        conds.join(" AND ")
    );
    let rows = bind_all(sqlx::query(&sql), &args)
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    let mut summaries = rows
        .iter()
        .map(summary_from_row)
        .collect::<Result<Vec<_>>>()?;

    let next_cursor = if summaries.len() as i64 > limit as i64 {
        summaries.truncate(limit);
        summaries
            .last()
            .map(|t| encode_event_cursor(&fmt_ts(t.ended_at), &t.trace_id))
    } else {
        None
    };
    attach_models(pool, project, &mut summaries).await?;
    Ok(TracePage {
        traces: summaries,
        next_cursor,
    })
}

/// Fill each summary's `models` with the trace's distinct models in first-seen (min-ts) order — the
/// same ordering [`lighttrack_core::Trace::from_events`] produces for the detail view, so list and
/// detail can't disagree. One extra query for the whole page (not N+1), scoped to the trace ids
/// actually returned and to the caller's project.
async fn attach_models(
    pool: &PgPool,
    project: Option<&str>,
    summaries: &mut [TraceSummary],
) -> Result<()> {
    if summaries.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = summaries.iter().map(|s| s.trace_id.clone()).collect();
    // Group to one row per (trace, model) with that model's first timestamp, then order globally by
    // it; pushing rows in that order builds each trace's list in first-seen order.
    let sql = "SELECT trace_id, model FROM \
         (SELECT trace_id, model, MIN(ts) AS mt FROM events \
          WHERE trace_id = ANY($1) AND ($2::text IS NULL OR project_id = $2) \
          GROUP BY trace_id, model) g ORDER BY mt ASC";
    let rows = sqlx::query(sql)
        .bind(&ids)
        .bind(project.map(|p| p.to_string()))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    let mut by_trace: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows {
        let trace_id: String = row.try_get(0).map_err(pgerr)?;
        let model: String = row.try_get(1).map_err(pgerr)?;
        by_trace.entry(trace_id).or_default().push(model);
    }
    for s in summaries.iter_mut() {
        if let Some(models) = by_trace.remove(&s.trace_id) {
            s.models = models;
        }
    }
    Ok(())
}

/// Build the summary through [`TraceShape`] rather than re-deriving duration/status here: the
/// aggregate's only job is to supply the shape's two endpoints; the rule that turns them into a
/// number lives in one place, shared with the detail rollup.
fn summary_from_row(row: &PgRow) -> Result<TraceSummary> {
    let started: String = row.try_get(2).map_err(pgerr)?;
    let ended: String = row.try_get(3).map_err(pgerr)?;
    let finish_ms: i64 = row.try_get(4).map_err(pgerr)?;
    let input_tokens: i64 = row.try_get(7).map_err(pgerr)?;
    let output_tokens: i64 = row.try_get(8).map_err(pgerr)?;
    let errors: i64 = row.try_get(9).map_err(pgerr)?;
    let started_at = parse_ts(&started)?;
    let ended_at = parse_ts(&ended)?;
    let last_finish = Utc
        .timestamp_millis_opt(finish_ms)
        .single()
        .unwrap_or(ended_at);
    let shape = TraceShape {
        started_at,
        last_finish,
        errors: errors as usize,
    };
    Ok(TraceSummary {
        trace_id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        started_at,
        ended_at,
        duration_ms: shape.duration_ms(),
        spans: row.try_get::<i64, _>(5).map_err(pgerr)? as usize,
        cost_usd: row.try_get(6).map_err(pgerr)?,
        input_tokens: input_tokens as u64,
        output_tokens: output_tokens as u64,
        total_tokens: (input_tokens + output_tokens) as u64,
        errors: errors as usize,
        status: shape.status(),
        models: Vec::new(),
    })
}

/// Every event of one trace within `project`, oldest first (the order the rollup expects).
///
/// Bounded at `max_spans` (the oldest, so the trace keeps its head): a runaway agent loop can
/// otherwise put unbounded spans behind one id. One extra row is fetched instead of always counting;
/// only when the cap actually bites does a `COUNT(*)` report the true span count, so a clipped read
/// is reported as clipped rather than served as a whole trace.
pub(crate) async fn list_by_trace(
    pool: &PgPool,
    project: Option<&str>,
    trace_id: &str,
    max_spans: usize,
) -> Result<TraceEvents> {
    // `($2::text IS NULL OR project_id = $2)` keeps one statement for both scopes; the cast is what
    // lets Postgres infer the parameter's type from a NULL bind.
    let where_clause = "WHERE trace_id = $1 AND ($2::text IS NULL OR project_id = $2)";
    let fetch = (max_spans as i64).saturating_add(1);
    let sql = format!("SELECT {EVENT_COLS} FROM events {where_clause} ORDER BY ts ASC LIMIT $3");
    let scope = project.map(|p| p.to_string());
    let rows = sqlx::query(&sql)
        .bind(trace_id.to_string())
        .bind(scope.clone())
        .bind(fetch)
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    let mut events = rows
        .iter()
        .map(event_from_row)
        .collect::<Result<Vec<_>>>()?;

    if events.len() as i64 <= max_spans as i64 {
        let total = events.len();
        return Ok(TraceEvents { events, total });
    }
    events.truncate(max_spans);
    let count_sql = format!("SELECT COUNT(*)::bigint FROM events {where_clause}");
    let total: i64 = sqlx::query(&count_sql)
        .bind(trace_id.to_string())
        .bind(scope)
        .fetch_one(pool)
        .await
        .map_err(pgerr)?
        .try_get(0)
        .map_err(pgerr)?;
    Ok(TraceEvents {
        events,
        total: total as usize,
    })
}

/// Scores attached to any event within a trace, newest first. A score links to a trace transitively
/// through its `event_id` (`scores.event_id` → `events.trace_id`), so both per-call scores and a
/// whole-trace score (anchored to the root span) surface without a per-score `trace_id` column.
/// Scoped by `project` on the *event*, matching the trace read.
pub(crate) async fn list_scores_by_trace(
    pool: &PgPool,
    project: Option<&str>,
    trace_id: &str,
) -> Result<Vec<Score>> {
    let cols = SCORE_COLS
        .split(", ")
        .map(|c| format!("s.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {cols} FROM scores s JOIN events e ON s.event_id = e.id \
         WHERE e.trace_id = $1 AND ($2::text IS NULL OR e.project_id = $2) \
         ORDER BY s.created_at DESC"
    );
    let rows = sqlx::query(&sql)
        .bind(trace_id.to_string())
        .bind(project.map(|p| p.to_string()))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter().map(score_from_row).collect()
}
