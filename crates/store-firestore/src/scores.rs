//! `scores` collection.

use serde_json::{json, Value};

use lighttrack_core::{Score, ScoreDetail, ScoreKind};
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
    // Verdict provenance as a JSON string field (as on the SQL backends) — read back whole with the
    // score and never filtered on, so it needs no Firestore map/index.
    let detail = match &s.detail {
        Some(d) if !d.is_empty() => {
            Some(serde_json::to_string(d).map_err(lighttrack_store::StoreError::from)?)
        }
        _ => None,
    };
    m.insert("detail".into(), json!(detail));
    m.insert("run_id".into(), json!(s.run_id));
    m.insert("case_index".into(), json!(s.case_index.map(|i| i as i64)));
    m.insert("scored_by".into(), json!(s.scored_by));
    m.insert("cost_usd".into(), json!(s.cost_usd));
    m.insert("created_at".into(), json!(fmt_ts(s.created_at)));
    // The typed identity (M9-C). Written as its own fields, not folded into `rubric`, so the
    // six encodings that string carries stop being the only way to tell verdicts apart.
    m.insert("rubric_id".into(), json!(s.rubric_id));
    m.insert("kind".into(), json!(s.kind.as_str()));
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

/// Every case result recorded for one benchmark run, in case order (unindexed cases last).
///
/// Filtering on `run_id` while ordering on `case_index` would need a composite index a self-hosted
/// deployment has to create by hand, so the query is a single-field `EQUAL` (auto-indexed) and the
/// ordering is applied here. The result set is one run's cases — bounded by the caller's `limit`.
pub(crate) fn list_run_scores(
    rest: &Rest,
    run_id: &str,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<Score>> {
    let filters: Vec<(&str, &str, Value)> = vec![("run_id", "EQUAL", json!(run_id))];
    let docs = rest.query("scores", &filters, None, None)?;
    let mut cases: Vec<Score> = docs
        .iter()
        .map(score_from)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        // Authorization scope, applied to the same field the SQL backends put in their WHERE clause.
        .filter(|s| project.is_none_or(|p| s.project_id == p))
        .collect();
    cases.sort_by(|a, b| {
        a.case_index
            .is_none()
            .cmp(&b.case_index.is_none())
            .then(a.case_index.cmp(&b.case_index))
            .then(a.created_at.cmp(&b.created_at))
    });
    cases.truncate(limit);
    Ok(cases)
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
        // An unreadable provenance blob degrades to `None` (the scalar verdict is still true)
        // rather than erroring the whole listing.
        detail: fstr(m, "detail").and_then(|j| serde_json::from_str::<ScoreDetail>(&j).ok()),
        run_id: fstr(m, "run_id"),
        case_index: fi64(m, "case_index").map(|i| i as u32),
        scored_by: freq(m, "scored_by")?,
        cost_usd: ff64(m, "cost_usd"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
        rubric_id: fstr(m, "rubric_id"),
        // A kind this binary does not know reads as `Other`; an absent one as `Freeform`, the
        // pre-typing default. Neither errors the listing.
        kind: match fstr(m, "kind") {
            None => ScoreKind::Freeform,
            Some(k) => ScoreKind::parse(&k).unwrap_or(ScoreKind::Other),
        },
    })
}
