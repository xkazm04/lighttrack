//! `labels` + `calibrations` collections (M11): the human verdict ledger and the trust lookup.
//!
//! Two Firestore-shaped compromises, both deliberate and both narrower than they look:
//!
//! * The keyset tiebreak is applied here rather than in the query, because Firestore cannot express
//!   the `(ts < c) OR (ts = c AND id < i)` disjunction a keyset needs — the same treatment
//!   [`crate::alerts`] gives the alert ledger.
//! * [`labels_for_dataset`] cannot join, so it reads the dataset's items and then queries labels by
//!   subject id in chunks. A golden set is a bounded, curated collection (tens to low hundreds of
//!   cases), so this is a handful of round trips — not the per-item walk the SQL backends avoid
//!   with a subquery, but far from a collection scan, and it is the honest answer rather than an
//!   empty list that would calibrate a judge against nothing.

use serde_json::{json, Value};

use lighttrack_core::{CalibrationRecord, Label, LabelFilter, LabelSubject, ScoreDim};
use lighttrack_store::codec::{decode_event_cursor, fmt_ts, parse_ts};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

const LABELS: &str = "labels";
const CALIBRATIONS: &str = "calibrations";
/// Firestore caps an `IN` filter's operand list; the item ids are chunked to stay under it.
const IN_CHUNK: usize = 30;

pub(crate) fn insert_label(rest: &Rest, l: &Label) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(l.id));
    m.insert("project_id".into(), json!(l.project_id));
    m.insert("subject_kind".into(), json!(l.subject.kind()));
    m.insert("subject_id".into(), json!(l.subject.id()));
    m.insert("rubric_id".into(), json!(l.rubric_id));
    m.insert("value".into(), json!(l.value));
    m.insert("pass".into(), json!(l.pass.map(|b| b as i64)));
    let dims = if l.dimensions.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&l.dimensions).map_err(lighttrack_store::StoreError::from)?)
    };
    m.insert("dimensions".into(), json!(dims));
    m.insert("labeler".into(), json!(l.labeler));
    m.insert("note".into(), json!(l.note));
    m.insert("created_at".into(), json!(fmt_ts(l.created_at)));
    rest.put_doc(LABELS, &l.id, &m)
}

pub(crate) fn list_labels(rest: &Rest, f: &LabelFilter) -> Result<Vec<Label>> {
    let mut filters: Vec<(&str, &str, Value)> = Vec::new();
    if let Some(p) = &f.project {
        filters.push(("project_id", "EQUAL", json!(p)));
    }
    if let Some(s) = &f.subject {
        filters.push(("subject_kind", "EQUAL", json!(s.kind())));
        filters.push(("subject_id", "EQUAL", json!(s.id())));
    }
    if let Some(r) = &f.rubric_id {
        filters.push(("rubric_id", "EQUAL", json!(r)));
    }
    let cursor = f.cursor.as_deref().and_then(decode_event_cursor);
    if let Some((ts, _)) = &cursor {
        // `<=`, not `<`: the id tiebreak below is what separates same-instant rows.
        filters.push(("created_at", "LESS_THAN_OR_EQUAL", json!(ts)));
    }
    let want = f.effective_limit();
    let docs = rest.query(
        LABELS,
        &filters,
        Some(("created_at", true)),
        Some(want.saturating_add(8)),
    )?;
    let mut out = Vec::new();
    for d in &docs {
        let Some(l) = label_from(d)? else { continue };
        if let Some((ts, id)) = &cursor {
            let l_ts = fmt_ts(l.created_at);
            if l_ts > *ts || (l_ts == *ts && l.id.as_str() >= id.as_str()) {
                continue;
            }
        }
        out.push(l);
        if out.len() == want {
            break;
        }
    }
    Ok(out)
}

/// Every label on any item of `dataset_id`, oldest-first.
pub(crate) fn labels_for_dataset(
    rest: &Rest,
    project: Option<&str>,
    dataset_id: &str,
) -> Result<Vec<Label>> {
    if crate::datasets::get_dataset(rest, project, dataset_id)?.is_none() {
        return Ok(Vec::new());
    }
    let items = rest.query(
        "dataset_items",
        &[("dataset_id", "EQUAL", json!(dataset_id))],
        None,
        None,
    )?;
    let ids: Vec<String> = items.iter().filter_map(|m| fstr(m, "id")).collect();
    let mut out: Vec<Label> = Vec::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let docs = rest.query(
            LABELS,
            &[
                ("subject_kind", "EQUAL", json!("dataset_item")),
                ("subject_id", "IN", json!(chunk)),
            ],
            None,
            None,
        )?;
        for d in &docs {
            if let Some(l) = label_from(d)? {
                out.push(l);
            }
        }
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// `Ok(None)` for a subject kind this binary does not know: a newer writer's row is skipped rather
/// than misfiled as an event label.
fn label_from(m: &Fields) -> Result<Option<Label>> {
    let kind = freq(m, "subject_kind")?;
    let subject_id = freq(m, "subject_id")?;
    let Some(subject) = LabelSubject::from_parts(&kind, &subject_id) else {
        return Ok(None);
    };
    Ok(Some(Label {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        subject,
        rubric_id: fstr(m, "rubric_id"),
        value: ff64(m, "value").unwrap_or(0.0),
        pass: fi64(m, "pass").map(|v| v != 0),
        // An unreadable breakdown degrades to "no breakdown" rather than erroring the listing.
        dimensions: fstr(m, "dimensions")
            .and_then(|j| serde_json::from_str::<Vec<ScoreDim>>(&j).ok())
            .unwrap_or_default(),
        labeler: freq(m, "labeler")?,
        note: fstr(m, "note"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
    }))
}

pub(crate) fn insert_calibration(rest: &Rest, c: &CalibrationRecord) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(c.id));
    m.insert("project_id".into(), json!(c.project_id));
    m.insert("judge".into(), json!(c.judge));
    m.insert("rubric_id".into(), json!(c.rubric_id));
    m.insert("dataset_id".into(), json!(c.dataset_id));
    m.insert(
        "dataset_version".into(),
        json!(c.dataset_version.map(|v| v as i64)),
    );
    m.insert("kappa".into(), json!(c.kappa));
    m.insert("pearson".into(), json!(c.pearson));
    m.insert("mae".into(), json!(c.mae));
    m.insert("rmse".into(), json!(c.rmse));
    m.insert("n".into(), json!(c.n as i64));
    m.insert("kappa_bar".into(), json!(c.kappa_bar));
    m.insert("trusted".into(), json!(c.trusted as i64));
    m.insert("created_at".into(), json!(fmt_ts(c.created_at)));
    rest.put_doc(CALIBRATIONS, &c.id, &m)
}

/// The newest record for exactly this `(project, rubric_id, judge)`.
///
/// Firestore has no `IS NULL` operator, so the rubric match is applied in Rust over a
/// `(project, judge)` query: a rubric never inherits the freeform measurement, and expressing that
/// with a `rubric_id EQUAL null` filter is exactly the sort of thing that silently matches
/// everything on one backend and nothing on another.
pub(crate) fn latest_calibration(
    rest: &Rest,
    project: &str,
    rubric_id: Option<&str>,
    judge: &str,
) -> Result<Option<CalibrationRecord>> {
    let docs = rest.query(
        CALIBRATIONS,
        &[
            ("project_id", "EQUAL", json!(project)),
            ("judge", "EQUAL", json!(judge)),
        ],
        None,
        None,
    )?;
    let mut matching: Vec<CalibrationRecord> = docs
        .iter()
        .map(calibration_from)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|c| c.rubric_id.as_deref() == rubric_id)
        .collect();
    matching.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(matching.into_iter().next())
}

pub(crate) fn list_calibrations(
    rest: &Rest,
    project: Option<&str>,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Vec<CalibrationRecord>> {
    let mut filters: Vec<(&str, &str, Value)> = Vec::new();
    if let Some(p) = project {
        filters.push(("project_id", "EQUAL", json!(p)));
    }
    let cur = cursor.and_then(decode_event_cursor);
    if let Some((ts, _)) = &cur {
        filters.push(("created_at", "LESS_THAN_OR_EQUAL", json!(ts)));
    }
    let want = match limit {
        0 => 100,
        n => n.min(1000),
    };
    let docs = rest.query(
        CALIBRATIONS,
        &filters,
        Some(("created_at", true)),
        Some(want.saturating_add(8)),
    )?;
    let mut out = Vec::new();
    for d in &docs {
        let c = calibration_from(d)?;
        if let Some((ts, id)) = &cur {
            let c_ts = fmt_ts(c.created_at);
            if c_ts > *ts || (c_ts == *ts && c.id.as_str() >= id.as_str()) {
                continue;
            }
        }
        out.push(c);
        if out.len() == want {
            break;
        }
    }
    Ok(out)
}

fn calibration_from(m: &Fields) -> Result<CalibrationRecord> {
    Ok(CalibrationRecord {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        judge: freq(m, "judge")?,
        rubric_id: fstr(m, "rubric_id"),
        dataset_id: fstr(m, "dataset_id"),
        dataset_version: fi64(m, "dataset_version").map(|v| v.max(0) as u32),
        kappa: ff64(m, "kappa").unwrap_or(0.0),
        pearson: ff64(m, "pearson").unwrap_or(0.0),
        mae: ff64(m, "mae").unwrap_or(0.0),
        rmse: ff64(m, "rmse").unwrap_or(0.0),
        n: fi64(m, "n").unwrap_or(0).max(0) as u32,
        kappa_bar: ff64(m, "kappa_bar").unwrap_or(0.0),
        trusted: fbool(m, "trusted"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
    })
}
