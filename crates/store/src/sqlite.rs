//! SQLite-backed [`Store`] — the local-development backend (bundled SQLite, no external service).

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use lighttrack_core::{
    ApiKey, LimitAction, LimitMetric, LimitRule, LimitWindow, LlmEvent, Operation, Project,
    Provider, Redaction, Score, Status, TokenUsage,
};

use crate::{CostRow, Result, Store, StoreError, Usage};

const SCHEMA: &str = include_str!("../../../schema/sqlite/001_init.sql");

const EVENT_COLS: &str = "id, project_id, trace_id, span_id, parent_span_id, ts, provider, model, \
    operation, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, cost_usd, \
    latency_ms, status, error, input, output, tags, source, metadata";

const SCORE_COLS: &str = "id, project_id, event_id, rubric, value, max, pass, reasoning, \
    scored_by, cost_usd, created_at";

/// Fixed-width, UTC, nanosecond RFC3339 (e.g. `2026-05-31T00:07:14.110948400Z`).
/// Fixed width => lexicographic ordering matches chronological ordering, so `ts` range
/// filters and `ORDER BY ts` are correct as plain string comparisons.
fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .map_err(|e| StoreError::Other(format!("bad ts {s:?}: {e}")))?
        .with_timezone(&Utc))
}

/// Serialize a string-valued enum to its on-disk string form (e.g. `LimitMetric::CostUsd` -> "cost_usd").
fn enum_to_str<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_value(v)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| StoreError::Other("enum did not serialize to a string".into()))
}

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

    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        let conn = self.conn.lock().unwrap();
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

    fn create_project(&self, p: &Project) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, enabled, redaction, created_at) \
             VALUES (?1,?2,?3,?4,?5)",
            params![
                p.id,
                p.name,
                p.enabled as i64,
                enum_to_str(&p.redaction)?,
                fmt_ts(p.created_at),
            ],
        )?;
        Ok(())
    }

    fn get_project(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, enabled, redaction, created_at FROM projects WHERE id = ?1")?;
        let raw = stmt.query_row(params![id], map_project_raw).optional()?;
        raw.map(project_from_raw).transpose()
    }

    fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, enabled, redaction, created_at FROM projects ORDER BY created_at DESC",
        )?;
        let raws = stmt
            .query_map([], map_project_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(project_from_raw).collect()
    }

    fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO api_keys \
             (id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                k.id,
                k.project_id,
                k.name,
                k.prefix,
                k.key_hash,
                fmt_ts(k.created_at),
                k.last_used_at.map(fmt_ts),
                k.revoked as i64,
            ],
        )?;
        Ok(())
    }

    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, name, prefix, key_hash, created_at, last_used_at, revoked \
             FROM api_keys WHERE prefix = ?1",
        )?;
        let raw = stmt.query_row(params![prefix], map_api_key_raw).optional()?;
        raw.map(api_key_from_raw).transpose()
    }

    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1",
            params![id, fmt_ts(when)],
        )?;
        Ok(())
    }

    fn create_limit_rule(&self, r: &LimitRule) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO limit_rules (id, project_id, metric, window, threshold, action, enabled) \
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                r.id,
                r.project_id,
                enum_to_str(&r.metric)?,
                enum_to_str(&r.window)?,
                r.threshold,
                enum_to_str(&r.action)?,
                r.enabled as i64,
            ],
        )?;
        Ok(())
    }

    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
        let conn = self.conn.lock().unwrap();
        let sql = if only_enabled {
            "SELECT id, project_id, metric, window, threshold, action, enabled \
             FROM limit_rules WHERE project_id = ?1 AND enabled = 1"
        } else {
            "SELECT id, project_id, metric, window, threshold, action, enabled \
             FROM limit_rules WHERE project_id = ?1"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![project], map_limit_rule)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {EVENT_COLS} FROM events WHERE id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let raw = stmt.query_row(params![id], map_raw_event).optional()?;
        raw.map(raw_to_event).transpose()
    }

    fn insert_score(&self, s: &Score) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO scores \
             (id, project_id, event_id, rubric, value, max, pass, reasoning, scored_by, cost_usd, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                s.id,
                s.project_id,
                s.event_id,
                s.rubric,
                s.value,
                s.max,
                s.pass.map(|b| b as i64),
                s.reasoning,
                s.scored_by,
                s.cost_usd,
                fmt_ts(s.created_at),
            ],
        )?;
        Ok(())
    }

    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        let conn = self.conn.lock().unwrap();
        let raws: Vec<ScoreRaw> = if let Some(p) = project {
            let sql = format!(
                "SELECT {SCORE_COLS} FROM scores WHERE project_id = ?1 \
                 ORDER BY created_at DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![p, limit as i64], map_score_raw)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let sql =
                format!("SELECT {SCORE_COLS} FROM scores ORDER BY created_at DESC LIMIT ?1");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![limit as i64], map_score_raw)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        raws.into_iter().map(score_from_raw).collect()
    }
}

// --- row mappers / converters for the Phase 2 tables ---

type ProjectRaw = (String, String, i64, String, String);

fn map_project_raw(row: &Row) -> rusqlite::Result<ProjectRaw> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn project_from_raw(r: ProjectRaw) -> Result<Project> {
    Ok(Project {
        id: r.0,
        name: r.1,
        enabled: r.2 != 0,
        redaction: parse_enum::<Redaction>(&r.3),
        created_at: parse_ts(&r.4)?,
    })
}

type ApiKeyRaw = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
);

fn map_api_key_raw(row: &Row) -> rusqlite::Result<ApiKeyRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn api_key_from_raw(r: ApiKeyRaw) -> Result<ApiKey> {
    Ok(ApiKey {
        id: r.0,
        project_id: r.1,
        name: r.2,
        prefix: r.3,
        key_hash: r.4,
        created_at: parse_ts(&r.5)?,
        last_used_at: match r.6 {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        revoked: r.7 != 0,
    })
}

fn map_limit_rule(row: &Row) -> rusqlite::Result<LimitRule> {
    Ok(LimitRule {
        id: row.get(0)?,
        project_id: row.get(1)?,
        metric: parse_enum::<LimitMetric>(&row.get::<_, String>(2)?),
        window: parse_enum::<LimitWindow>(&row.get::<_, String>(3)?),
        threshold: row.get(4)?,
        action: parse_enum::<LimitAction>(&row.get::<_, String>(5)?),
        enabled: row.get::<_, i64>(6)? != 0,
    })
}

type ScoreRaw = (
    String,
    String,
    Option<String>,
    String,
    f64,
    f64,
    Option<i64>,
    Option<String>,
    String,
    Option<f64>,
    String,
);

fn map_score_raw(row: &Row) -> rusqlite::Result<ScoreRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn score_from_raw(r: ScoreRaw) -> Result<Score> {
    Ok(Score {
        id: r.0,
        project_id: r.1,
        event_id: r.2,
        rubric: r.3,
        value: r.4,
        max: r.5,
        pass: r.6.map(|v| v != 0),
        reasoning: r.7,
        scored_by: r.8,
        cost_usd: r.9,
        created_at: parse_ts(&r.10)?,
    })
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

    #[test]
    fn projects_keys_limits_usage() {
        let s = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();

        let proj = Project {
            id: "p1".into(),
            name: "demo".into(),
            enabled: true,
            redaction: Redaction::None,
            created_at: now,
        };
        s.create_project(&proj).unwrap();
        assert_eq!(s.list_projects().unwrap().len(), 1);
        assert!(s.get_project("p1").unwrap().is_some());
        assert!(s.get_project("nope").unwrap().is_none());

        let key = ApiKey {
            id: "k1".into(),
            project_id: "p1".into(),
            name: "default".into(),
            prefix: "abc12345".into(),
            key_hash: "salt:hash".into(),
            created_at: now,
            last_used_at: None,
            revoked: false,
        };
        s.create_api_key(&key).unwrap();
        assert_eq!(s.find_api_key_by_prefix("abc12345").unwrap().unwrap().project_id, "p1");
        assert!(s.find_api_key_by_prefix("zzz").unwrap().is_none());

        let rule = LimitRule {
            id: "r1".into(),
            project_id: "p1".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Hour,
            threshold: 0.005,
            action: LimitAction::Alert,
            enabled: true,
        };
        s.create_limit_rule(&rule).unwrap();
        assert_eq!(s.list_limit_rules("p1", true).unwrap().len(), 1);

        s.insert_event(&ev("p1", "claude-haiku-4-5", 1000, 500, 0.0035)).unwrap();
        s.insert_event(&ev("p1", "claude-haiku-4-5", 2000, 200, 0.00165)).unwrap();

        let u = s.usage_since("p1", LimitWindow::Hour.since(Utc::now())).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!(u.tokens, 3700);
        assert!((u.cost_usd - 0.00515).abs() < 1e-9);
        assert!(rule.evaluate(u.cost_usd).breached); // 0.00515 >= 0.005
    }
}
