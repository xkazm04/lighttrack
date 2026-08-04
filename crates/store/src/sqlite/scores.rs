//! Scores: insert and list LLM-as-judge results.

use rusqlite::{params, Connection, Row};

use lighttrack_core::{Score, ScoreDetail};

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "id, project_id, event_id, rubric, value, max, pass, reasoning, detail, \
    run_id, case_index, scored_by, cost_usd, created_at";

pub(super) fn insert(conn: &Connection, s: &Score) -> Result<()> {
    // Verdict provenance rides as JSON in one column: it is read back whole with the score and never
    // filtered on, so a per-dimension table would buy nothing but joins.
    let detail = match &s.detail {
        Some(d) if !d.is_empty() => Some(serde_json::to_string(d)?),
        _ => None,
    };
    conn.execute(
        "INSERT INTO scores \
         (id, project_id, event_id, rubric, value, max, pass, reasoning, detail, run_id, \
          case_index, scored_by, cost_usd, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            s.id,
            s.project_id,
            s.event_id,
            s.rubric,
            s.value,
            s.max,
            s.pass.map(|b| b as i64),
            s.reasoning,
            detail,
            s.run_id,
            s.case_index.map(|i| i as i64),
            s.scored_by,
            s.cost_usd,
            fmt_ts(s.created_at),
        ],
    )?;
    Ok(())
}

/// Every case result recorded for one benchmark run, in case order. Rides `idx_scores_run`.
/// `project` is the caller's authorization scope (`None` = admin, all projects); passing it here
/// rather than filtering afterwards keeps a project key from reading another project's run.
pub(super) fn list_by_run(
    conn: &Connection,
    run_id: &str,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<Score>> {
    // `case_index IS NULL` first in the ORDER BY pins NULL placement identically on every backend
    // (SQLite sorts NULLs first, Postgres last), so the conformance suite can assert one order.
    let sql = format!(
        "SELECT {COLS} FROM scores WHERE run_id = ?1 AND (?2 IS NULL OR project_id = ?2) \
         ORDER BY (case_index IS NULL), case_index, created_at LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![run_id, project, limit as i64], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// Scores attached to any event within a trace, newest first. A score links to a trace transitively
/// through its `event_id` (join `scores.event_id` → `events.trace_id`), so no per-score `trace_id`
/// column is needed — both per-call scores and a whole-trace score (anchored to the root span) surface.
///
/// Scoped by `project` (on the *event*, matching the trace read) so a colliding `trace_id` in another
/// project can never contribute its verdicts here. `None` reads across projects (operator principals).
pub(super) fn list_by_trace(
    conn: &Connection,
    project: Option<&str>,
    trace_id: &str,
) -> Result<Vec<Score>> {
    let sql = format!(
        "SELECT {} FROM scores s JOIN events e ON s.event_id = e.id \
         WHERE e.trace_id = ?1 AND (?2 IS NULL OR e.project_id = ?2) ORDER BY s.created_at DESC",
        prefixed_cols("s")
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![trace_id, project], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(from_raw).collect()
}

/// `COLS` with each column qualified by `alias` (for joins that share column names across tables).
fn prefixed_cols(alias: &str) -> String {
    COLS.split(", ").map(|c| format!("{alias}.{c}")).collect::<Vec<_>>().join(", ")
}

pub(super) fn list(conn: &Connection, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
    let raws: Vec<ScoreRaw> = if let Some(p) = project {
        let sql = format!(
            "SELECT {COLS} FROM scores WHERE project_id = ?1 ORDER BY created_at DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![p, limit as i64], map_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    } else {
        let sql = format!("SELECT {COLS} FROM scores ORDER BY created_at DESC LIMIT ?1");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![limit as i64], map_raw)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    raws.into_iter().map(from_raw).collect()
}

/// The subset of `event_ids` that already carry at least one score. `event_id IN (...)` rides
/// `idx_scores_event`; scoped to the given ids so it never full-scans the scores table.
pub(super) fn scored_event_ids(conn: &Connection, event_ids: &[String]) -> Result<Vec<String>> {
    if event_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat("?").take(event_ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT DISTINCT event_id FROM scores WHERE event_id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::ToSql> =
        event_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let ids = stmt
        .query_map(bound.as_slice(), |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
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
    Option<String>,
    Option<String>,
    Option<i64>,
    String,
    Option<f64>,
    String,
);

fn map_raw(row: &Row) -> rusqlite::Result<ScoreRaw> {
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
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn from_raw(r: ScoreRaw) -> Result<Score> {
    // A detail blob written by a newer/other writer must not sink the whole listing: an unreadable
    // one degrades to `None` (the score's scalar is still true) rather than erroring the query.
    let detail = r
        .8
        .as_deref()
        .and_then(|j| serde_json::from_str::<ScoreDetail>(j).ok());
    Ok(Score {
        id: r.0,
        project_id: r.1,
        event_id: r.2,
        rubric: r.3,
        value: r.4,
        max: r.5,
        pass: r.6.map(|v| v != 0),
        reasoning: r.7,
        detail,
        run_id: r.9,
        case_index: r.10.map(|i| i as u32),
        scored_by: r.11,
        cost_usd: r.12,
        created_at: parse_ts(&r.13)?,
    })
}
