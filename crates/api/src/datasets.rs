//! Datasets (Phase 3.6b) — curated case collections, freezable for reproducible runs.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{new_id, Dataset, DatasetItem};

use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateDatasetReq {
    name: String,
    #[serde(default)]
    source: Option<String>,
}

pub(crate) async fn create_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateDatasetReq>,
) -> Result<Json<Dataset>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let d = Dataset {
        id: new_id(),
        project_id: pid,
        name: req.name,
        version: 1,
        frozen: false,
        source: req.source,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let d2 = d.clone();
    spawn_db(move || store.create_dataset(&d2)).await?;
    Ok(Json(d))
}

pub(crate) async fn list_datasets(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Dataset>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_datasets(&pid)).await?;
    Ok(Json(v))
}

async fn load_dataset_authorized(
    st: &AppState,
    p: &Principal,
    id: &str,
) -> Result<Dataset, ApiError> {
    let store = st.store.clone();
    let id2 = id.to_string();
    let d = spawn_db(move || store.get_dataset(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("dataset '{id}' not found")))?;
    if let Principal::Project(pid) = p {
        if &d.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that dataset"));
        }
    }
    Ok(d)
}

pub(crate) async fn get_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Dataset>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    Ok(Json(load_dataset_authorized(&st, &p, &id).await?))
}

pub(crate) async fn add_dataset_item(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mut item): Json<DatasetItem>,
) -> Result<Json<DatasetItem>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    if ds.frozen {
        return Err(ApiError::conflict("dataset is frozen"));
    }
    item.dataset_id = id;
    let store = st.store.clone();
    let item2 = item.clone();
    spawn_db(move || store.create_dataset_item(&item2)).await?;
    Ok(Json(item))
}

pub(crate) async fn list_dataset_items(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<DatasetItem>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    load_dataset_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let items = spawn_db(move || store.list_dataset_items(&id)).await?;
    Ok(Json(items))
}

pub(crate) async fn freeze_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Dataset>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let mut ds = load_dataset_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let id2 = id.clone();
    spawn_db(move || store.set_dataset_frozen(&id2, true)).await?;
    ds.frozen = true;
    Ok(Json(ds))
}
