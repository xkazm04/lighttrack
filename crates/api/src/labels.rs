//! The human verdict ledger's HTTP surface (M11): `POST /v1/labels`, `GET /v1/labels`,
//! `POST /v1/datasets/:id/items/from-label`, `GET /v1/calibrations`, `POST /v1/calibrations`.
//!
//! A label is ground truth, so it is deliberately not a [`Score`](lighttrack_core::Score): it is
//! never budgeted, never costed, and never alerted on. The one thing it *is* required to carry is
//! `labeler` — a calibration result whose provenance cannot be reconstructed is a number nobody can
//! defend, which is how D15's "n=12 and ours" caveat came about.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use lighttrack_core::{
    new_id, CalibrationRecord, DatasetItem, Label, LabelFilter, LabelSubject, Scope, ScoreDim,
};

use crate::auth::Principal;
use crate::auth_scopes::ensure_scope;
use crate::datasets::load_dataset_authorized;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// A posted label. `project_id` is derived from a project key and required from an admin one, the
/// same asymmetry every keyed write here has.
#[derive(Deserialize)]
pub(crate) struct CreateLabelReq {
    #[serde(default)]
    project_id: Option<String>,
    /// `"event:<id>"` / `"dataset_item:<id>"` / `"score:<id>"`.
    subject: String,
    #[serde(default)]
    rubric_id: Option<String>,
    value: f64,
    #[serde(default)]
    pass: Option<bool>,
    #[serde(default)]
    dimensions: Vec<ScoreDim>,
    labeler: String,
    #[serde(default)]
    note: Option<String>,
}

pub(crate) async fn create_label(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateLabelReq>,
) -> Result<Json<Label>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = write_project(&p, req.project_id.as_deref())?;
    let subject = LabelSubject::parse(&req.subject).ok_or_else(|| {
        ApiError::bad_request(
            "subject must be '<kind>:<id>' with kind one of event, dataset_item, score",
        )
    })?;
    if req.labeler.trim().is_empty() {
        return Err(ApiError::bad_request(
            "labeler is required: a human verdict with no attribution cannot be audited",
        ));
    }
    let l = Label {
        id: new_id(),
        project_id: project,
        subject,
        rubric_id: req.rubric_id,
        value: req.value,
        pass: req.pass,
        dimensions: req.dimensions,
        labeler: req.labeler,
        note: req.note,
        created_at: Utc::now(),
    }
    .capped();
    let store = st.store.clone();
    let l2 = l.clone();
    spawn_db(move || store.insert_label(&l2)).await?;
    Ok(Json(l))
}

/// A project key may write a label when it carries `manage`; an admin key may write any project's.
///
/// `manage` rather than `ingest`: a label is a *configuration* of what "good" means for this
/// project, not traffic. An ingest key that could quietly move ground truth would let the thing
/// being measured edit the measurement.
fn write_project(p: &Principal, body_project: Option<&str>) -> Result<String, ApiError> {
    match p {
        Principal::Project { project_id, .. } => {
            ensure_scope(p, Scope::Manage)?;
            if let Some(b) = body_project {
                if b != project_id {
                    return Err(ApiError::forbidden("key not authorized for that project"));
                }
            }
            Ok(project_id.clone())
        }
        _ => body_project
            .map(str::to_string)
            .ok_or_else(|| ApiError::bad_request("project_id is required with an admin key")),
    }
}

#[derive(Deserialize)]
pub(crate) struct LabelsQuery {
    project: Option<String>,
    subject: Option<String>,
    rubric_id: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

pub(crate) async fn list_labels(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LabelsQuery>,
) -> Result<Json<Value>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let subject = match q.subject.as_deref() {
        // A malformed subject is a 400, never an unnarrowed page: answering the whole ledger to a
        // question about one event is worse than saying the question was wrong.
        Some(s) => Some(LabelSubject::parse(s).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown subject '{s}'; expected '<kind>:<id>' with kind one of event, \
                 dataset_item, score"
            ))
        })?),
        None => None,
    };
    let filter = LabelFilter {
        project,
        subject,
        rubric_id: q.rubric_id,
        limit: q.limit.unwrap_or(0),
        cursor: q.cursor,
    };
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_labels(&filter)).await?;
    let next = rows.last().map(|l| {
        lighttrack_store::codec::encode_event_cursor(
            &lighttrack_store::codec::fmt_ts(l.created_at),
            &l.id,
        )
    });
    Ok(Json(json!({ "labels": rows, "next_cursor": next })))
}

#[derive(Deserialize)]
pub(crate) struct FromLabelReq {
    /// The label to promote. Its subject must be an event — a dataset item is already in a set, and
    /// a score is a verdict rather than a case.
    label_id: String,
}

/// `POST /v1/datasets/:id/items/from-label` — promote a labelled production event into a golden set.
///
/// The step that closes the loop: traffic gets graded in review, and the graded case becomes a
/// permanent eval case with its human verdict attached, instead of the grade evaporating in a
/// spreadsheet. The label is **copied onto the new item** (as a fresh `dataset_item` label), not
/// moved, so the original event keeps its own grade and the item's provenance names the event.
pub(crate) async fn item_from_label(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<FromLabelReq>,
) -> Result<Json<DatasetItem>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    if ds.frozen {
        return Err(ApiError::conflict("dataset is frozen"));
    }

    let label = find_label(&st, &ds.project_id, &req.label_id).await?;
    let LabelSubject::Event(event_id) = &label.subject else {
        return Err(ApiError::bad_request(
            "only a label on an event can be promoted into a dataset: a dataset_item label is \
             already in a set, and a score label is a verdict rather than a case",
        ));
    };
    let store = st.store.clone();
    let ev_id = event_id.clone();
    let ev = spawn_db(move || store.get_event(&ev_id))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("event '{event_id}' not found")))?;

    let item = DatasetItem {
        id: new_id(),
        dataset_id: ds.id.clone(),
        input: as_text(ev.input.as_ref()).unwrap_or_default(),
        output: as_text(ev.output.as_ref()),
        // The human's verdict is what makes this a *golden* case, so the expectation the case
        // carries is the labeler's note when there is one — never a guess.
        expected: label.note.clone(),
        context: None,
        tags: vec!["from-label".to_string()],
        source_event_id: Some(event_id.clone()),
        anonymization: Value::Null,
    };
    let store = st.store.clone();
    let item2 = item.clone();
    spawn_db(move || store.create_dataset_item(&item2)).await?;

    // Copy the grade onto the new item. Without this the promoted case is an input with no ground
    // truth — which is exactly the state that made a "golden set" un-calibratable in the first place.
    let copied = Label {
        id: new_id(),
        project_id: ds.project_id.clone(),
        subject: LabelSubject::DatasetItem(item.id.clone()),
        rubric_id: label.rubric_id.clone(),
        value: label.value,
        pass: label.pass,
        dimensions: label.dimensions.clone(),
        labeler: label.labeler.clone(),
        note: label.note.clone(),
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    spawn_db(move || store.insert_label(&copied)).await?;
    Ok(Json(item))
}

/// A stored event payload as case text. A plain JSON string is the text itself; anything richer (a
/// message array, a structured request) is re-serialized rather than stringified through `Display`,
/// which would produce Rust's debug shape and make the case unrunnable.
fn as_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

/// Look one label up by id, within a project. The store has no `get_label` — the ledger is listed,
/// not addressed — so this is a bounded scan of the project's newest page rather than a new trait
/// method every backend would have to port for one call site.
async fn find_label(st: &AppState, project: &str, id: &str) -> Result<Label, ApiError> {
    let filter = LabelFilter {
        project: Some(project.to_string()),
        limit: LabelFilter::MAX_LIMIT,
        ..Default::default()
    };
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_labels(&filter)).await?;
    rows.into_iter()
        .find(|l| l.id == id)
        .ok_or_else(|| ApiError::not_found(format!("label '{id}' not found in this project")))
}

#[derive(Deserialize)]
pub(crate) struct CalibrationsQuery {
    project: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

pub(crate) async fn list_calibrations(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CalibrationsQuery>,
) -> Result<Json<Value>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let store = st.store.clone();
    let (limit, cursor) = (q.limit.unwrap_or(0), q.cursor);
    let rows =
        spawn_db(move || store.list_calibrations(project.as_deref(), limit, cursor.as_deref()))
            .await?;
    let next = rows.last().map(|c| {
        lighttrack_store::codec::encode_event_cursor(
            &lighttrack_store::codec::fmt_ts(c.created_at),
            &c.id,
        )
    });
    Ok(Json(json!({ "calibrations": rows, "next_cursor": next })))
}

/// `POST /v1/calibrations` — the runner records a completed measurement.
///
/// Admin or a `manage` project key, for the same reason a label is: this row is what a gate reads
/// to decide whether a promotion may happen, so a key that can only send traffic must not be able
/// to declare its own judge trustworthy.
pub(crate) async fn create_calibration(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut rec): Json<CalibrationRecord>,
) -> Result<Json<CalibrationRecord>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let body_project = (!rec.project_id.is_empty()).then(|| rec.project_id.clone());
    rec.project_id = write_project(&p, body_project.as_deref())?;
    if rec.judge.trim().is_empty() {
        return Err(ApiError::bad_request("judge is required"));
    }
    if rec.id.is_empty() {
        rec.id = new_id();
    }
    let store = st.store.clone();
    let rec2 = rec.clone();
    spawn_db(move || store.insert_calibration(&rec2)).await?;
    Ok(Json(rec))
}

#[cfg(test)]
#[path = "tests_labels.rs"]
mod tests;
