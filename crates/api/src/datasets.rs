//! Datasets (Phase 3.6b) — curated case collections, freezable so a run's *input* is fixed.
//! Freezing alone is not comparability across time: see the note on `Dataset` in core, which
//! records why an imported dataset and a traffic-sampled one age differently.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{new_id, Dataset, DatasetItem};

use crate::auth::Principal;
use crate::difficulty_input::{parse_stated_tier, StatedItem};
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

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
        // A dataset created directly is the root of its own lineage; only a fork has a parent.
        parent_id: None,
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
    let v = spawn_db(move || store.list_datasets(TenantScope::Project(&pid))).await?;
    Ok(Json(v))
}

pub(crate) async fn load_dataset_authorized(
    st: &AppState,
    p: &Principal,
    id: &str,
) -> Result<Dataset, ApiError> {
    let store = st.store.clone();
    let id2 = id.to_string();
    let sc = p.scope_owned();
    // The scope IS the authorization (M17): another project's dataset is not found, not refused.
    spawn_db(move || store.get_dataset(sc.as_deref().into(), &id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("dataset '{id}' not found")))
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
    Json(stated): Json<StatedItem>,
) -> Result<Json<DatasetItem>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    // Graded here, before the store is touched, and refused rather than degraded: the operator
    // typed this rung seconds ago, so accepting `expert` with a 200 and storing NULL writes a case
    // that reads as ungraded into a corpus whose per-tier report will not say so. The *read* path
    // keeps degrading — see `crate::difficulty_input`.
    let mut item = stated.into_item().map_err(ApiError::bad_request)?;
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

/// `?difficulty=` narrows a listing to one tier (M27).
#[derive(Deserialize)]
pub(crate) struct ItemsQuery {
    #[serde(default)]
    difficulty: Option<String>,
}

pub(crate) async fn list_dataset_items(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ItemsQuery>,
) -> Result<Json<Vec<DatasetItem>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    // Parsed BEFORE the read, and refused rather than ignored: an operator who asked for `hard` and
    // got the whole set back would read a mixed corpus as the hard tier. The same reason `lt
    // datasets import` refuses an unknown --strategy instead of falling back to `recent`.
    //
    // Shares `parse_stated_tier` with the write path, so the two surfaces cannot answer the same
    // typo with two different messages — which is how the asymmetry this replaced went unnoticed.
    let tier = match q
        .difficulty
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => Some(parse_stated_tier(s).map_err(ApiError::bad_request)?),
        None => None,
    };
    load_dataset_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let sc = p.scope_owned();
    let items = spawn_db(move || store.list_dataset_items(sc.as_deref().into(), &id)).await?;
    // Filtered here rather than in the `Store` trait, deliberately. A dataset is a curated corpus
    // that this method already returns whole and unpaginated, so pushing the predicate down would
    // buy nothing and would add a filter argument three backends could each implement, forget, or
    // quietly ignore — which is how a filter becomes advisory. One shared predicate cannot skew for
    // one backend. Ungraded cases are excluded from every tier, because `None` is not a tier.
    Ok(Json(match tier {
        Some(t) => items
            .into_iter()
            .filter(|i| i.difficulty == Some(t))
            .collect(),
        None => items,
    }))
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
    let sc = p.scope_owned();
    spawn_db(move || store.set_dataset_frozen(sc.as_deref().into(), &id2, true)).await?;
    ds.frozen = true;
    Ok(Json(ds))
}
