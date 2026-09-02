//! The human verdict ledger's HTTP surface (M11): `POST /v1/labels`, `GET /v1/labels`,
//! `POST /v1/datasets/:id/items/from-label`, `GET /v1/calibrations`, `POST /v1/calibrations`.
//!
//! A label is ground truth, so it is deliberately not a [`Score`](lighttrack_core::Score): it is
//! never budgeted, never costed, and never alerted on. The one thing it *is* required to carry is
//! `labeler` — a calibration result whose provenance cannot be reconstructed is a number nobody can
//! defend, which is how D15's "n=12 and ours" caveat came about.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use lighttrack_core::{
    new_id, CalibrationRecord, Label, LabelFilter, LabelSubject, Scope, ScoreDim,
};

use crate::auth::Principal;
use crate::auth_scopes::ensure_scope;
use crate::error::ApiError;
use crate::guards::{authenticate, resolve_read_project};
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
