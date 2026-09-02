//! Rubrics (Phase 3.6c) — structured, multi-dimension judging criteria.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{new_id, Rubric, RubricDimension};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateRubricReq {
    name: String,
    dimensions: Vec<RubricDimension>,
    #[serde(default = "default_rubric_threshold")]
    threshold: f64,
}

fn default_rubric_threshold() -> f64 {
    0.7
}

pub(crate) async fn create_rubric(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateRubricReq>,
) -> Result<Json<Rubric>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let r = Rubric {
        id: new_id(),
        project_id: pid,
        name: req.name,
        dimensions: req.dimensions,
        threshold: req.threshold,
        // Every rubric starts at generation 1; POST /v1/rubrics/:id/versions mints the next.
        version: 1,
        supersedes: None,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let r2 = r.clone();
    spawn_db(move || store.create_rubric(&r2)).await?;
    Ok(Json(r))
}

pub(crate) async fn list_rubrics(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Rubric>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_rubrics(&pid)).await?;
    Ok(Json(v))
}

pub(crate) async fn get_rubric(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Rubric>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    let r = spawn_db(move || store.get_rubric(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("rubric '{id}' not found")))?;
    if let Principal::Project {
        project_id: pid, ..
    } = &p
    {
        if &r.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that rubric"));
        }
    }
    Ok(Json(r))
}

/// Body for `POST /v1/rubrics/:id/versions` — a copy-with-changes of an existing rubric.
///
/// Both fields are optional: a version that only re-weights the dimensions should not have to
/// restate the threshold, and one that only moves the threshold should not have to restate the
/// dimensions. Whatever is omitted is carried forward from the rubric being superseded.
#[derive(Deserialize)]
pub(crate) struct NewVersionReq {
    dimensions: Option<Vec<RubricDimension>>,
    threshold: Option<f64>,
}

/// `POST /v1/rubrics/:id/versions` — the next generation of a rubric.
///
/// A **new row with a new id**, linked back to the old one, never a mutation. Verdicts already
/// stored cite the old rubric's id, and editing that row underneath them would silently change what
/// those verdicts claim to have measured — a restatement of history exactly like the one the revenue
/// upsert refuses. The old rubric stays readable and stays cited.
///
/// Deliberately **no** `active` flag here: promoting a version behind a calibration gate is M11's
/// job, and a flag shipped now would be an ungated promotion switch wearing the same name.
pub(crate) async fn create_rubric_version(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<NewVersionReq>,
) -> Result<Json<Rubric>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let id2 = id.clone();
    let prev = spawn_db(move || store.get_rubric(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("rubric '{id}' not found")))?;

    let dimensions = req.dimensions.unwrap_or_else(|| prev.dimensions.clone());
    if dimensions.is_empty() {
        return Err(ApiError::bad_request(
            "a rubric version needs at least one dimension",
        ));
    }
    let next = prev.next_version(dimensions, req.threshold.unwrap_or(prev.threshold));

    let store = st.store.clone();
    let to_insert = next.clone();
    spawn_db(move || store.create_rubric(&to_insert)).await?;
    Ok(Json(next))
}
