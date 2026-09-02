//! Versioned eval corpora (M24): forking a dataset, mining rows into one, and reading the version
//! history of a name.
//!
//! Split from [`crate::datasets`] rather than added to it, because these three are about *lineage*
//! rather than CRUD — and because the CRUD module is the one M17 is rewriting to carry a scope on
//! every read. The three handlers here already pass their scope explicitly.
//!
//! The reason this surface exists at all: `Dataset::version` was written once as `1` and never
//! updated, so freezing was terminal. The only way to extend a golden set was to build a *different*
//! one, which is the exact case the runner's paired-test guard exists to refuse — and could not see,
//! because both sides said `1`.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};

use lighttrack_core::{Dataset, ImportSpec};

use crate::datasets::load_dataset_authorized;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// `POST /v1/datasets/:id/fork` — the next version of this dataset's name.
///
/// Admin-only, like every other write to the eval corpus: a fork mints a new corpus that benchmark
/// runs will be pinned to, which is a configuration change, not observability traffic.
pub(crate) async fn fork_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Dataset>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    // Authorize against the dataset the caller named before touching the store's own scope, so a
    // project key gets the same 403/404 shape it gets from every other dataset route.
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    let scope = Some(ds.project_id.clone());
    let store = st.store.clone();
    let forked = spawn_db(move || store.fork_dataset(scope.as_deref(), &ds.id)).await?;
    Ok(Json(forked))
}

/// The count an import reports. A bare number on the wire would be ambiguous about *which* number
/// it is — matched, or actually written after dedupe — and those differ by design.
#[derive(Debug, Serialize)]
pub(crate) struct ImportOutcome {
    pub(crate) dataset_id: String,
    /// Cases written. With `dedupe`, a matched row that duplicates one already in the set is not
    /// written and is not counted — `0` is a legitimate, successful answer.
    pub(crate) imported: u32,
}

/// `POST /v1/datasets/:id/items/import` — mine stored rows into this dataset.
///
/// 409 on a frozen target, from the store's own `Conflict` (the same answer `POST …/items` gives),
/// because appending to the corpus a finished run was scored against rewrites that run's meaning.
/// The mined text is scrubbed on the way in by the store's shared import path, so an imported case
/// is anonymized exactly as `lt-runner dataset build` anonymizes a sampled one.
pub(crate) async fn import_dataset_items(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(spec): Json<ImportSpec>,
) -> Result<Json<ImportOutcome>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    if ds.frozen {
        return Err(ApiError::conflict(
            "dataset is frozen; fork it to add cases",
        ));
    }
    let scope = Some(ds.project_id.clone());
    let dsid = ds.id.clone();
    let store = st.store.clone();
    let imported =
        spawn_db(move || store.import_dataset_items(scope.as_deref(), &dsid, &spec)).await?;
    Ok(Json(ImportOutcome {
        dataset_id: ds.id,
        imported,
    }))
}

#[derive(Deserialize)]
pub(crate) struct VersionsQuery {
    /// The dataset name to walk. A query parameter rather than a path segment because a dataset name
    /// is operator-chosen free text and routinely contains a `/` (`golden/checkout`), which a path
    /// segment would split into a 404 nobody could explain.
    name: String,
}

/// `GET /v1/projects/:id/datasets/versions?name=…` — every version of one dataset name, newest
/// first.
///
/// The read that answers "which corpus was that run actually scored against": a `dataset_pin` names
/// a version, and until this route existed there was no way to resolve one back to its cases.
pub(crate) async fn list_dataset_versions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Query(q): Query<VersionsQuery>,
) -> Result<Json<Vec<Dataset>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let scope = resolve_read_project(&p, Some(&pid))?;
    let name = q.name;
    if name.trim().is_empty() {
        return Err(ApiError::bad_request("'name' is required"));
    }
    let store = st.store.clone();
    let v = spawn_db(move || store.list_dataset_versions(scope.as_deref(), &name)).await?;
    Ok(Json(v))
}

#[cfg(test)]
#[path = "tests_dataset_lineage.rs"]
mod tests;
