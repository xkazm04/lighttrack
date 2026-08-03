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
    if let Principal::Project { project_id: pid, .. } = &p {
        if &r.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that rubric"));
        }
    }
    Ok(Json(r))
}
