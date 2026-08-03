//! Events: ingest, listing, cost rollups, rolling-window usage, single lookup.

use serde_json::Value;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{LimitScope, LlmEvent, Operation, Provider, Status, TokenUsage};
use lighttrack_store::codec::{decode_event_cursor, encode_event_cursor};
use lighttrack_store::{
    CostRow, EventFilter, EventPage, Result, ScopeUsage, StoreError, Usage, UseCaseCostRow,
};

use crate::util::{fmt_ts, parse_enum, parse_ts, pgerr};

const COLS: &str = "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, \
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
    insert_query(ev)?.execute(pool).await.map_err(|e| insert_err(e, &ev.id))?;
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
    .bind(ev.provider.as_str())
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

pub(crate) async fn list(pool: &PgPool, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
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
            sqlx::query(&format!("SELECT {COLS} FROM events ORDER BY ts DESC LIMIT $1"))
                .bind(limit as i64)
                .fetch_all(pool)
                .await
        }
    }
    .map_err(pgerr)?;
    rows.iter().map(from_row).collect()
}

pub(crate) async fn cost_summary(pool: &PgPool, project: Option<&str>) -> Result<Vec<CostRow>> {
    cost_summary_windowed(pool, project, None, None).await
}

/// Cost/usage rollup over an optional `[since, until)` window. Same grouping/ordering as
/// [`cost_summary`]; window bounds compare against the fixed-width `ts` string.
pub(crate) async fn cost_summary_windowed(
    pool: &PgPool,
    project: Option<&str>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<CostRow>> {
    let cols = "project_id, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0)::bigint AS it, COALESCE(SUM(output_tokens),0)::bigint AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost";
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        binds.push(p.to_string());
        conds.push(format!("project_id = ${}", binds.len()));
    }
    if let Some(s) = since {
        binds.push(fmt_ts(s));
        conds.push(format!("ts >= ${}", binds.len()));
    }
    if let Some(u) = until {
        binds.push(fmt_ts(u));
        conds.push(format!("ts < ${}", binds.len()));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conds.join(" AND "))
    };
    let sql = format!(
        "SELECT {cols} FROM events {where_clause}\
         GROUP BY project_id, provider, model ORDER BY cost DESC"
    );
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter()
        .map(|row| {
            Ok(CostRow {
                project_id: row.try_get(0).map_err(pgerr)?,
                provider: row.try_get(1).map_err(pgerr)?,
                model: row.try_get(2).map_err(pgerr)?,
                calls: row.try_get(3).map_err(pgerr)?,
                input_tokens: row.try_get(4).map_err(pgerr)?,
                output_tokens: row.try_get(5).map_err(pgerr)?,
                cost_usd: row.try_get(6).map_err(pgerr)?,
            })
        })
        .collect()
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
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        binds.push(p.to_string());
        conds.push(format!("project_id = ${}", binds.len()));
    }
    if let Some(s) = filter.since {
        binds.push(fmt_ts(s));
        conds.push(format!("ts >= ${}", binds.len()));
    }
    if let Some(u) = filter.until {
        binds.push(fmt_ts(u));
        conds.push(format!("ts < ${}", binds.len()));
    }
    for (col, v) in [
        ("provider", &filter.provider),
        ("model", &filter.model),
        ("trace_id", &filter.trace_id),
        ("name", &filter.name),
    ] {
        if let Some(v) = v {
            binds.push(v.clone());
            conds.push(format!("{col} = ${}", binds.len()));
        }
    }
    if let Some(cursor) = &filter.cursor {
        let (cts, cid) = decode_event_cursor(cursor)
            .ok_or_else(|| StoreError::Other(format!("invalid cursor {cursor:?}")))?;
        binds.push(cts);
        let i = binds.len();
        binds.push(cid);
        let j = binds.len();
        // Strictly after (cts, cid) in DESC (ts, id) order.
        conds.push(format!("(ts < ${i} OR (ts = ${i} AND id < ${j}))"));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conds.join(" AND "))
    };
    // Over-fetch by one so we can tell whether another page exists without a second COUNT query.
    let fetch = (limit as i64).saturating_add(1);
    let sql = format!(
        "SELECT {COLS} FROM events {where_clause}ORDER BY ts DESC, id DESC LIMIT ${}",
        binds.len() + 1
    );
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.bind(fetch).fetch_all(pool).await.map_err(pgerr)?;
    let mut events = rows.iter().map(from_row).collect::<Result<Vec<LlmEvent>>>()?;
    let next_cursor = if events.len() as i64 > limit as i64 {
        events.truncate(limit);
        events.last().map(|e| encode_event_cursor(&fmt_ts(e.ts), &e.id))
    } else {
        None
    };
    Ok(EventPage { events, next_cursor, total: None })
}

/// The SQL expression yielding one scope dimension's value for a row — columns for the three
/// original dimensions, a `jsonb` extraction for the two that ride in `metadata` (`api_key_id` is
/// server-stamped at ingest, `customer_id` is the billing linkage). The `NULLIF(metadata,'')::jsonb`
/// cast is the same one [`USAGE_COLS`] uses; see the note there.
///
/// Fixed literals chosen by the enum discriminant — never user input, so safe to interpolate. Values
/// are always bound.
pub(crate) fn scope_expr(kind: &str) -> Option<&'static str> {
    match kind {
        "provider" => Some("provider"),
        "model" => Some("model"),
        "name" => Some("name"),
        "api_key" => Some("(NULLIF(metadata,'')::jsonb)->>'api_key_id'"),
        "customer" => Some("(NULLIF(metadata,'')::jsonb)->>'customer_id'"),
        _ => None,
    }
}

/// Rolling usage restricted to one scope dimension (provider / model / use-case name / API key /
/// billing customer). A NULL dimension never matches, mirroring the SQLite reference.
pub(crate) async fn usage_since_scoped(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
    scope: &LimitScope,
) -> Result<Usage> {
    let expr = scope_expr(scope.kind_str()).unwrap_or("NULL");
    let sql = format!(
        // `{RECEIVED}`, not `ts`: a scoped window is still a window, and a backdated client clock
        // must not slide spend out of it. `{expr}` generalizes the dimension to the two that ride
        // in `metadata` as well as the three that are columns.
        "SELECT {USAGE_COLS} FROM events \
         WHERE project_id = $1 AND {RECEIVED} >= $2 AND {expr} = $3"
    );
    let row = sqlx::query(&sql)
        .bind(project.to_string())
        .bind(fmt_ts(since))
        .bind(scope.value().to_string())
        .fetch_one(pool)
        .await
        .map_err(pgerr)?;
    map_usage(&row)
}

/// Rolling usage since `since` grouped by every distinct value of one scope dimension — the
/// pre-breach "who is spending" view. Rows carrying no value on the dimension fold into a single
/// `NULL` bucket rather than being dropped, so the parts still sum to the project total.
pub(crate) async fn usage_by_scope(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
    kind: &str,
) -> Result<Vec<ScopeUsage>> {
    let expr = scope_expr(kind)
        .ok_or_else(|| StoreError::Other(format!("unknown scope dimension '{kind}'")))?;
    let sql = format!(
        "SELECT {expr} AS k, {USAGE_COLS} FROM events \
         WHERE project_id = $1 AND ts >= $2 GROUP BY k ORDER BY 2 DESC"
    );
    let rows = sqlx::query(&sql)
        .bind(project.to_string())
        .bind(fmt_ts(since))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter()
        .map(|r| {
            Ok(ScopeUsage {
                value: r.try_get(0).map_err(pgerr)?,
                usage: Usage {
                    cost_usd: r.try_get(1).map_err(pgerr)?,
                    calls: r.try_get(2).map_err(pgerr)?,
                    tokens: r.try_get(3).map_err(pgerr)?,
                    unpriced_calls: r.try_get(4).map_err(pgerr)?,
                    client_cost_usd: r.try_get(5).map_err(pgerr)?,
                },
            })
        })
        .collect()
}

/// Use-case rollup grouped by (name, provider, model), optionally windowed by `since`. Un-named
/// calls group together per model; ordered by cost, most expensive first.
pub(crate) async fn usecase_costs(
    pool: &PgPool,
    project: Option<&str>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<UseCaseCostRow>> {
    let cols = "name, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0)::bigint AS it, COALESCE(SUM(output_tokens),0)::bigint AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost";
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        binds.push(p.to_string());
        conds.push(format!("project_id = ${}", binds.len()));
    }
    if let Some(s) = since {
        binds.push(fmt_ts(s));
        conds.push(format!("ts >= ${}", binds.len()));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conds.join(" AND "))
    };
    let sql = format!(
        "SELECT {cols} FROM events {where_clause}GROUP BY name, provider, model ORDER BY cost DESC"
    );
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter()
        .map(|row| {
            Ok(UseCaseCostRow {
                name: row.try_get(0).map_err(pgerr)?,
                provider: row.try_get(1).map_err(pgerr)?,
                model: row.try_get(2).map_err(pgerr)?,
                calls: row.try_get(3).map_err(pgerr)?,
                input_tokens: row.try_get(4).map_err(pgerr)?,
                output_tokens: row.try_get(5).map_err(pgerr)?,
                cost_usd: row.try_get(6).map_err(pgerr)?,
            })
        })
        .collect()
}

pub(crate) async fn usage_since(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Usage> {
    let row = sqlx::query(&format!(
        "SELECT {USAGE_COLS} FROM events WHERE project_id = $1 AND {RECEIVED} >= $2"
    ))
    .bind(project.to_string())
    .bind(fmt_ts(since))
    .fetch_one(pool)
    .await
    .map_err(pgerr)?;
    map_usage(&row)
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<LlmEvent>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM events WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    match row {
        Some(r) => Ok(Some(from_row(&r)?)),
        None => Ok(None),
    }
}

fn from_row(row: &PgRow) -> Result<LlmEvent> {
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
        provider: parse_enum::<Provider>(&provider),
        model: row.try_get(7).map_err(pgerr)?,
        name: row.try_get(22).map_err(pgerr)?,
        operation: parse_enum::<Operation>(&operation),
        usage: TokenUsage {
            input: row.try_get::<i64, _>(9).map_err(pgerr)? as u64,
            output: row.try_get::<i64, _>(10).map_err(pgerr)? as u64,
            cached_input: row.try_get::<Option<i64>, _>(11).map_err(pgerr)?.map(|v| v as u64),
            reasoning: row.try_get::<Option<i64>, _>(12).map_err(pgerr)?.map(|v| v as u64),
        },
        cost_usd: row.try_get(13).map_err(pgerr)?,
        latency_ms: row.try_get::<Option<i64>, _>(14).map_err(pgerr)?.map(|v| v as u64),
        status: parse_enum::<Status>(&status),
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
