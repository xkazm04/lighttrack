//! Scores: insert and list LLM-as-judge results.

use rusqlite::{params, Connection, Row};

use lighttrack_core::Score;

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const COLS: &str = "id, project_id, event_id, rubric, value, max, pass, reasoning, \
    scored_by, cost_usd, created_at";

pub(super) fn insert(conn: &Connection, s: &Score) -> Result<()> {
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

/// Scores attached to any event within a trace, newest first. A score links to a trace transitively
/// through its `event_id` (join `scores.event_id` → `events.trace_id`), so no per-score `trace_id`
/// column is needed — both per-call scores and a whole-trace score (anchored to the root span) surface.
pub(super) fn list_by_trace(conn: &Connection, trace_id: &str) -> Result<Vec<Score>> {
    let sql = format!(
        "SELECT {} FROM scores s JOIN events e ON s.event_id = e.id \
         WHERE e.trace_id = ?1 ORDER BY s.created_at DESC",
        prefixed_cols("s")
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![trace_id], map_raw)?
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
    ))
}

fn from_raw(r: ScoreRaw) -> Result<Score> {
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
