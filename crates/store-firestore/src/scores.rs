//! `scores` collection.

use serde_json::{json, Value};

use lighttrack_core::Score;
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn insert_score(rest: &Rest, s: &Score) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(s.id));
    m.insert("project_id".into(), json!(s.project_id));
    m.insert("event_id".into(), json!(s.event_id));
    m.insert("rubric".into(), json!(s.rubric));
    m.insert("value".into(), json!(s.value));
    m.insert("max".into(), json!(s.max));
    m.insert("pass".into(), json!(s.pass.map(|b| b as i64)));
    m.insert("reasoning".into(), json!(s.reasoning));
    m.insert("scored_by".into(), json!(s.scored_by));
    m.insert("cost_usd".into(), json!(s.cost_usd));
    m.insert("created_at".into(), json!(fmt_ts(s.created_at)));
    rest.put_doc("scores", &s.id, &m)
}

pub(crate) fn list_scores(rest: &Rest, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
    let filters: Vec<(&str, &str, Value)> = match project {
        Some(p) => vec![("project_id", "EQUAL", json!(p))],
        None => vec![],
    };
    let docs = rest.query("scores", &filters, Some(("created_at", true)), Some(limit))?;
    docs.iter().map(score_from).collect()
}

/// The subset of `event_ids` that already carry at least one score. Firestore has no server-side
/// anti-join, so we probe per id with the same single-field `EQUAL` query `list_scores` uses (a
/// single-field index is automatic). The caller passes only one page of event ids at a time, so this
/// stays a small, bounded number of point lookups — never a blind top-N scan of the collection.
pub(crate) fn scored_event_ids(rest: &Rest, event_ids: &[String]) -> Result<Vec<String>> {
    let mut scored = Vec::new();
    for id in event_ids {
        let filters: Vec<(&str, &str, Value)> = vec![("event_id", "EQUAL", json!(id))];
        // limit 1: we only need existence, not the score rows.
        if !rest.query("scores", &filters, None, Some(1))?.is_empty() {
            scored.push(id.clone());
        }
    }
    Ok(scored)
}

fn score_from(m: &Fields) -> Result<Score> {
    Ok(Score {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        event_id: fstr(m, "event_id"),
        rubric: freq(m, "rubric")?,
        value: ff64(m, "value").unwrap_or(0.0),
        max: ff64(m, "max").unwrap_or(1.0),
        pass: fi64(m, "pass").map(|v| v != 0),
        reasoning: fstr(m, "reasoning"),
        scored_by: freq(m, "scored_by")?,
        cost_usd: ff64(m, "cost_usd"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
    })
}
