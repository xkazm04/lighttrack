//! Events: ingest, listing, cost rollups, rolling-window usage, single lookup.

use serde_json::Value;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{LlmEvent, Operation, Provider, Status, TokenUsage};
use lighttrack_store::{CostRow, Result, Usage};

use crate::util::{fmt_ts, parse_enum, parse_ts, pgerr};

const COLS: &str = "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, \
    operation, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
    latency_ms, status, error, input, output, tags, source, metadata";

pub(crate) async fn insert(pool: &PgPool, ev: &LlmEvent) -> Result<()> {
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
    sqlx::query(
        "INSERT INTO events (id, project_id, trace_id, span_id, parent_span_id, ts, \
         provider, model, operation, input_tokens, output_tokens, cached_input_tokens, \
         reasoning_tokens, cost_usd, latency_ms, status, error, input, output, tags, \
         source, metadata) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)",
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
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
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
    let cols = "project_id, provider, model, COUNT(*) AS calls, \
        COALESCE(SUM(input_tokens),0)::bigint AS it, COALESCE(SUM(output_tokens),0)::bigint AS ot, \
        COALESCE(SUM(cost_usd),0.0) AS cost";
    let rows = match project {
        Some(p) => {
            sqlx::query(&format!(
                "SELECT {cols} FROM events WHERE project_id = $1 \
                 GROUP BY project_id, provider, model ORDER BY cost DESC"
            ))
            .bind(p.to_string())
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(&format!(
                "SELECT {cols} FROM events GROUP BY project_id, provider, model ORDER BY cost DESC"
            ))
            .fetch_all(pool)
            .await
        }
    }
    .map_err(pgerr)?;
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

pub(crate) async fn usage_since(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Usage> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(cost_usd),0.0), COUNT(*), \
         COALESCE(SUM(input_tokens + output_tokens),0)::bigint \
         FROM events WHERE project_id = $1 AND ts >= $2",
    )
    .bind(project.to_string())
    .bind(fmt_ts(since))
    .fetch_one(pool)
    .await
    .map_err(pgerr)?;
    Ok(Usage {
        cost_usd: row.try_get(0).map_err(pgerr)?,
        calls: row.try_get(1).map_err(pgerr)?,
        tokens: row.try_get(2).map_err(pgerr)?,
    })
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

    Ok(LlmEvent {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        trace_id: row.try_get(2).map_err(pgerr)?,
        span_id: row.try_get(3).map_err(pgerr)?,
        parent_span_id: row.try_get(4).map_err(pgerr)?,
        ts: parse_ts(&ts)?,
        provider: parse_enum::<Provider>(&provider),
        model: row.try_get(7).map_err(pgerr)?,
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
