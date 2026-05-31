//! SQLite-backed [`Store`] — the local-development backend (bundled SQLite, no external service).

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use serde::de::DeserializeOwned;
use serde_json::Value;

use lighttrack_core::{LlmEvent, Operation, Provider, Status, TokenUsage};

use crate::{CostRow, Result, Store, StoreError};

const SCHEMA: &str = include_str!("../../../schema/sqlite/001_init.sql");

const EVENT_COLS: &str = "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, \
    operation, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
    latency_ms, status, error, input, output, tags, source, metadata";

/// SQLite store. A single connection guarded by a mutex — fine for our throughput (≤1k calls/hr).
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating parent dirs and the file if needed) and ensure the schema exists.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let store = Self {
            conn: Mutex::new(Connection::open(path)?),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// In-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        store.init_schema()?;
        Ok(store)
    }
}

impl Store for SqliteStore {
    fn init_schema(&self) -> Result<()> {
        self.conn.lock().unwrap().execute_batch(SCHEMA)?;
        Ok(())
    }

    fn insert_event(&self, ev: &LlmEvent) -> Result<()> {
        let conn = self.conn.lock().unwrap();
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
              latency_ms, status, error, input, output, tags, source, metadata) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                ev.id,
                ev.project_id,
                ev.trace_id,
                ev.span_id,
                ev.parent_span_id,
                ev.ts.to_rfc3339(),
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
            ],
        )?;
        Ok(())
    }

    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        let conn = self.conn.lock().unwrap();
        let raws: Vec<RawEvent> = if let Some(p) = project {
            let sql = format!(
                "SELECT {EVENT_COLS} FROM events WHERE project_id = ?1 ORDER BY ts DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            // Bind to a local so the borrowing iterator drops before `stmt`.
            let rows = stmt
                .query_map(params![p, limit as i64], map_raw_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let sql = format!("SELECT {EVENT_COLS} FROM events ORDER BY ts DESC LIMIT ?1");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![limit as i64], map_raw_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        raws.into_iter().map(raw_to_event).collect()
    }

    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        let conn = self.conn.lock().unwrap();
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
            let v = stmt
                .query_map(params![p], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        } else {
            let sql = format!(
                "SELECT {cols} FROM events \
                 GROUP BY project_id, provider, model ORDER BY cost DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let v = stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        Ok(rows)
    }
}

/// Raw column values as stored, before reconstructing an [`LlmEvent`].
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
}

fn map_raw_event(row: &Row) -> rusqlite::Result<RawEvent> {
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
    })
}

fn raw_to_event(r: RawEvent) -> Result<LlmEvent> {
    let ts = DateTime::parse_from_rfc3339(&r.ts)
        .map_err(|e| StoreError::Other(format!("bad ts {:?}: {e}", r.ts)))?
        .with_timezone(&Utc);
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

/// Parse a stored enum string, falling back to the type's default on any mismatch.
fn parse_enum<T: DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_value(Value::String(s.to_string())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lighttrack_core::{new_id, Operation, Provider, Status, TokenUsage};

    fn ev(project: &str, model: &str, inp: u64, out: u64, cost: f64) -> LlmEvent {
        LlmEvent {
            id: new_id(),
            project_id: project.into(),
            trace_id: Some("trace-1".into()),
            span_id: None,
            parent_span_id: None,
            ts: Utc::now(),
            provider: Provider::Anthropic,
            model: model.into(),
            operation: Operation::Chat,
            usage: TokenUsage {
                input: inp,
                output: out,
                cached_input: None,
                reasoning: None,
            },
            cost_usd: Some(cost),
            latency_ms: Some(123),
            status: Status::Success,
            error: None,
            input: None,
            output: None,
            tags: vec!["smoke".into()],
            source: Some("test".into()),
            metadata: serde_json::json!({"k":"v"}),
        }
    }

    #[test]
    fn insert_list_cost_roundtrip() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.insert_event(&ev("p1", "claude-haiku-4-5", 100, 50, 0.001)).unwrap();
        s.insert_event(&ev("p1", "claude-haiku-4-5", 200, 80, 0.002)).unwrap();
        s.insert_event(&ev("p2", "claude-opus-4-8", 10, 5, 0.01)).unwrap();

        assert_eq!(s.list_events(None, 10).unwrap().len(), 3);

        let p1 = s.list_events(Some("p1"), 10).unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1[0].project_id, "p1");
        assert_eq!(p1[0].tags, vec!["smoke".to_string()]);
        assert_eq!(p1[0].metadata, serde_json::json!({"k":"v"}));

        let costs = s.cost_summary(Some("p1")).unwrap();
        assert_eq!(costs.len(), 1);
        assert_eq!(costs[0].calls, 2);
        assert_eq!(costs[0].input_tokens, 300);
        assert!((costs[0].cost_usd - 0.003).abs() < 1e-9);
    }
}
