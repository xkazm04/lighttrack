//! `events` collection: ingest + query + the client-side aggregations (Firestore has no GROUP BY/SUM).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{LimitScope, LlmEvent, TokenUsage};
use lighttrack_store::codec::{decode_event_cursor, encode_event_cursor};
use lighttrack_store::{CostRow, EventFilter, EventPage, Result, StoreError, Usage, UseCaseCostRow};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "events";

pub(crate) fn insert_event(rest: &Rest, ev: &LlmEvent) -> Result<()> {
    // Create, not upsert: a duplicate id is a Conflict (API 409), never a silent overwrite.
    rest.create_doc(COLL, &ev.id, &to_fields(ev)?)
}

pub(crate) fn get_event(rest: &Rest, id: &str) -> Result<Option<LlmEvent>> {
    rest.get_doc(COLL, id)?.as_ref().map(from_fields).transpose()
}

pub(crate) fn list_events(rest: &Rest, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
    let filters = project_filter(project);
    let docs = rest.query(COLL, &filters, Some(("ts", true)), Some(limit))?;
    docs.iter().map(from_fields).collect()
}

pub(crate) fn cost_summary(rest: &Rest, project: Option<&str>) -> Result<Vec<CostRow>> {
    cost_summary_windowed(rest, project, None, None)
}

/// Cost/usage rollup over an optional `[since, until)` window. The window bounds are pushed to the
/// server (same project+ts filter shape as [`usage_since`]); grouping stays client-side.
pub(crate) fn cost_summary_windowed(
    rest: &Rest,
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<Vec<CostRow>> {
    let mut filters = project_filter(project);
    if let Some(s) = since {
        filters.push(("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(s))));
    }
    if let Some(u) = until {
        filters.push(("ts", "LESS_THAN", json!(fmt_ts(u))));
    }
    let docs = rest.query(COLL, &filters, None, None)?;
    let mut agg: BTreeMap<(String, String, String), CostRow> = BTreeMap::new();
    for m in &docs {
        let pid = fstr(m, "project_id").unwrap_or_default();
        let provider = fstr(m, "provider").unwrap_or_default();
        let model = fstr(m, "model").unwrap_or_default();
        let row = agg
            .entry((pid.clone(), provider.clone(), model.clone()))
            .or_insert_with(|| CostRow {
                project_id: pid,
                provider,
                model,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
            });
        row.calls += 1;
        row.input_tokens += fi64(m, "input_tokens").unwrap_or(0);
        row.output_tokens += fi64(m, "output_tokens").unwrap_or(0);
        row.cost_usd += ff64(m, "cost_usd").unwrap_or(0.0);
    }
    let mut rows: Vec<CostRow> = agg.into_values().collect();
    rows.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
    Ok(rows)
}

pub(crate) fn usage_since(rest: &Rest, project: &str, since: DateTime<Utc>) -> Result<Usage> {
    let filters = vec![
        ("project_id", "EQUAL", json!(project)),
        ("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(since))),
    ];
    let docs = rest.query(COLL, &filters, None, None)?;
    let mut u = Usage::default();
    for m in &docs {
        u.cost_usd += ff64(m, "cost_usd").unwrap_or(0.0);
        u.calls += 1;
        u.tokens += fi64(m, "input_tokens").unwrap_or(0) + fi64(m, "output_tokens").unwrap_or(0);
    }
    Ok(u)
}

/// Filtered, keyset-paginated listing (newest first), paging on `(ts, id)` descending. Project and
/// ts-window filters are pushed to the server; the remaining equality predicates, the (ts, id) sort
/// tiebreak, and the cursor cut are applied client-side (a composite server-side `where` would need
/// per-combination Firestore indexes). Cursors are byte-identical to the other backends' (shared
/// codec), so a page sequence survives a backend migration.
pub(crate) fn list_events_filtered(
    rest: &Rest,
    project: Option<&str>,
    filter: &EventFilter,
    limit: usize,
) -> Result<EventPage> {
    let mut filters = project_filter(project);
    if let Some(s) = filter.since {
        filters.push(("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(s))));
    }
    if let Some(u) = filter.until {
        filters.push(("ts", "LESS_THAN", json!(fmt_ts(u))));
    }
    let docs = rest.query(COLL, &filters, None, None)?;
    let mut events = docs.iter().map(from_fields).collect::<Result<Vec<LlmEvent>>>()?;
    if let Some(p) = &filter.provider {
        events.retain(|e| e.provider.as_str() == p);
    }
    if let Some(m) = &filter.model {
        events.retain(|e| &e.model == m);
    }
    if let Some(t) = &filter.trace_id {
        events.retain(|e| e.trace_id.as_deref() == Some(t.as_str()));
    }
    if let Some(n) = &filter.name {
        events.retain(|e| e.name.as_deref() == Some(n.as_str()));
    }
    events.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
    if let Some(cursor) = &filter.cursor {
        let (cts, cid) = decode_event_cursor(cursor)
            .ok_or_else(|| StoreError::Other(format!("invalid cursor {cursor:?}")))?;
        // Strictly after (cts, cid) in DESC (ts, id) order.
        events.retain(|e| {
            let ets = fmt_ts(e.ts);
            ets < cts || (ets == cts && e.id < cid)
        });
    }
    let next_cursor = if events.len() > limit {
        events.truncate(limit);
        events.last().map(|e| encode_event_cursor(&fmt_ts(e.ts), &e.id))
    } else {
        None
    };
    Ok(EventPage { events, next_cursor })
}

/// Rolling usage restricted to one scope dimension. The project+window slice is served by the same
/// server-side filter as [`usage_since`]; the scope match is client-side. A missing `name` never
/// matches a name scope, mirroring the SQLite reference.
pub(crate) fn usage_since_scoped(
    rest: &Rest,
    project: &str,
    since: DateTime<Utc>,
    scope: &LimitScope,
) -> Result<Usage> {
    let filters = vec![
        ("project_id", "EQUAL", json!(project)),
        ("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(since))),
    ];
    let field = match scope {
        LimitScope::Provider(_) => "provider",
        LimitScope::Model(_) => "model",
        LimitScope::Name(_) => "name",
    };
    let docs = rest.query(COLL, &filters, None, None)?;
    let mut u = Usage::default();
    for m in &docs {
        if fstr(m, field).as_deref() != Some(scope.value()) {
            continue;
        }
        u.cost_usd += ff64(m, "cost_usd").unwrap_or(0.0);
        u.calls += 1;
        u.tokens += fi64(m, "input_tokens").unwrap_or(0) + fi64(m, "output_tokens").unwrap_or(0);
    }
    Ok(u)
}

/// Use-case rollup grouped by (name, provider, model), optionally windowed by `since`. Un-named
/// calls group together per model; ordered by cost, most expensive first. Client-side aggregation,
/// same as [`cost_summary`].
pub(crate) fn usecase_costs(
    rest: &Rest,
    project: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<UseCaseCostRow>> {
    let mut filters = project_filter(project);
    if let Some(s) = since {
        filters.push(("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(s))));
    }
    let docs = rest.query(COLL, &filters, None, None)?;
    let mut agg: BTreeMap<(Option<String>, String, String), UseCaseCostRow> = BTreeMap::new();
    for m in &docs {
        let name = fstr(m, "name");
        let provider = fstr(m, "provider").unwrap_or_default();
        let model = fstr(m, "model").unwrap_or_default();
        let row = agg
            .entry((name.clone(), provider.clone(), model.clone()))
            .or_insert_with(|| UseCaseCostRow {
                name,
                provider,
                model,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
            });
        row.calls += 1;
        row.input_tokens += fi64(m, "input_tokens").unwrap_or(0);
        row.output_tokens += fi64(m, "output_tokens").unwrap_or(0);
        row.cost_usd += ff64(m, "cost_usd").unwrap_or(0.0);
    }
    let mut rows: Vec<UseCaseCostRow> = agg.into_values().collect();
    rows.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
    Ok(rows)
}

fn project_filter(project: Option<&str>) -> Vec<(&str, &str, Value)> {
    match project {
        Some(p) => vec![("project_id", "EQUAL", json!(p))],
        None => vec![],
    }
}

fn to_fields(ev: &LlmEvent) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(ev.id));
    m.insert("project_id".into(), json!(ev.project_id));
    m.insert("trace_id".into(), json!(ev.trace_id));
    m.insert("span_id".into(), json!(ev.span_id));
    m.insert("parent_span_id".into(), json!(ev.parent_span_id));
    m.insert("ts".into(), json!(fmt_ts(ev.ts)));
    m.insert("provider".into(), json!(ev.provider.as_str()));
    m.insert("model".into(), json!(ev.model));
    m.insert("operation".into(), json!(ev.operation.as_str()));
    m.insert("input_tokens".into(), json!(ev.usage.input as i64));
    m.insert("output_tokens".into(), json!(ev.usage.output as i64));
    m.insert("cached_input_tokens".into(), json!(ev.usage.cached_input.map(|v| v as i64)));
    m.insert("reasoning_tokens".into(), json!(ev.usage.reasoning.map(|v| v as i64)));
    m.insert("cost_usd".into(), json!(ev.cost_usd));
    m.insert("latency_ms".into(), json!(ev.latency_ms.map(|v| v as i64)));
    m.insert("status".into(), json!(ev.status.as_str()));
    m.insert("error".into(), json!(ev.error));
    m.insert("input".into(), json!(opt_json_str(&ev.input)?));
    m.insert("output".into(), json!(opt_json_str(&ev.output)?));
    m.insert("tags".into(), json!(serde_json::to_string(&ev.tags)?));
    m.insert("source".into(), json!(ev.source));
    m.insert("metadata".into(), json!(json_or_null_str(&ev.metadata)?));
    m.insert("name".into(), json!(ev.name));
    Ok(m)
}

fn from_fields(m: &Fields) -> Result<LlmEvent> {
    Ok(LlmEvent {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        trace_id: fstr(m, "trace_id"),
        span_id: fstr(m, "span_id"),
        parent_span_id: fstr(m, "parent_span_id"),
        ts: parse_ts(&freq(m, "ts")?)?,
        provider: parse_enum(&freq(m, "provider")?),
        model: freq(m, "model")?,
        name: fstr(m, "name"),
        operation: parse_enum(&freq(m, "operation")?),
        usage: TokenUsage {
            input: fi64(m, "input_tokens").unwrap_or(0) as u64,
            output: fi64(m, "output_tokens").unwrap_or(0) as u64,
            cached_input: fi64(m, "cached_input_tokens").map(|v| v as u64),
            reasoning: fi64(m, "reasoning_tokens").map(|v| v as u64),
        },
        cost_usd: ff64(m, "cost_usd"),
        latency_ms: fi64(m, "latency_ms").map(|v| v as u64),
        status: parse_enum(&freq(m, "status")?),
        error: fstr(m, "error"),
        input: fopt_json(m, "input")?,
        output: fopt_json(m, "output")?,
        tags: match fstr(m, "tags") {
            Some(s) => serde_json::from_str(&s)?,
            None => Vec::new(),
        },
        source: fstr(m, "source"),
        metadata: fjson(m, "metadata")?,
    })
}
