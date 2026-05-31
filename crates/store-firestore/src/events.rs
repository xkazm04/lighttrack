//! `events` collection: ingest + query + the client-side aggregations (Firestore has no GROUP BY/SUM).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{LlmEvent, TokenUsage};
use lighttrack_store::{CostRow, Result, Usage};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "events";

pub(crate) fn insert_event(rest: &Rest, ev: &LlmEvent) -> Result<()> {
    rest.put_doc(COLL, &ev.id, &to_fields(ev)?)
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
    let docs = rest.query(COLL, &project_filter(project), None, None)?;
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
