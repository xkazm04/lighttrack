//! The use-case registry, and the coverage report that is the point of having one.
//!
//! CRUD here is unremarkable. `GET .../use-cases/coverage` is not: it is the difference between
//! what a project **declared** and what its events **did**, which is the one thing neither the
//! registry nor the telemetry can say alone.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, UseCase, UseCaseKind, UseCaseStatus};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct UpsertUseCaseReq {
    key: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    kind: UseCaseKind,
    #[serde(default)]
    status: UseCaseStatus,
    #[serde(default)]
    component: Option<String>,
    #[serde(default)]
    expected_models: Vec<String>,
    #[serde(default)]
    owner: Option<String>,
}

/// Register a call site, or correct one already registered.
///
/// Idempotent on `(project, key)` — a second registration is a correction, not a `409`. The
/// alternative would make "fix the description" mean "delete the row your events attribute to",
/// which is a worse operation to hand an operator than a silent overwrite.
pub(crate) async fn upsert_use_case(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<UpsertUseCaseReq>,
) -> Result<Json<UseCase>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let now = Utc::now();
    // Preserve the original `created_at` when correcting: the registration date is a fact about the
    // call site, not about the last time someone fixed a typo in its description.
    let store = st.store.clone();
    let (p, k) = (pid.clone(), req.key.clone());
    let existing = spawn_db(move || store.get_use_case(&p, &k)).await?;

    let u = UseCase {
        id: existing
            .as_ref()
            .map(|e| e.id.clone())
            .unwrap_or_else(new_id),
        project_id: pid,
        key: req.key,
        name: req.name,
        description: req.description,
        kind: req.kind,
        status: req.status,
        component: req.component,
        expected_models: req.expected_models,
        owner: req.owner,
        created_at: existing.map(|e| e.created_at).unwrap_or(now),
        updated_at: now,
    };
    u.validate().map_err(ApiError::bad_request)?;

    let store = st.store.clone();
    let u2 = u.clone();
    spawn_db(move || store.upsert_use_case(&u2)).await?;
    Ok(Json(u))
}

pub(crate) async fn list_use_cases(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<UseCase>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    Ok(Json(spawn_db(move || store.list_use_cases(&pid)).await?))
}

pub(crate) async fn get_use_case(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, key)): Path<(String, String)>,
) -> Result<Json<UseCase>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let (p2, k2) = (pid, key.clone());
    spawn_db(move || store.get_use_case(&p2, &k2))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("use case '{key}' not found")))
}

pub(crate) async fn delete_use_case(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, key)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let (p2, k2) = (pid, key.clone());
    let removed = spawn_db(move || store.delete_use_case(&p2, &k2)).await?;
    if !removed {
        return Err(ApiError::not_found(format!("use case '{key}' not found")));
    }
    Ok(Json(serde_json::json!({ "deleted": key })))
}

#[derive(Deserialize)]
pub(crate) struct CoverageQuery {
    /// RFC3339 lower bound on `ts`. Absent means all history.
    #[serde(default)]
    since: Option<String>,
}

/// One declared call site, with what its traffic actually did.
#[derive(Serialize)]
pub(crate) struct DeclaredRow {
    #[serde(flatten)]
    use_case: UseCase,
    /// Calls observed under this key.
    calls: u64,
    /// Models seen running under it that it never declared. Empty when it declared none — an absent
    /// declaration is not a violation, and reporting it as one would light up every project that
    /// has not filled the field in.
    undeclared_models: Vec<String>,
}

/// Traffic with no registry row behind it.
#[derive(Serialize)]
pub(crate) struct ShadowRow {
    /// The `events.name` observed. Empty string = calls that carried no name at all.
    name: String,
    calls: u64,
    models: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct Coverage {
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    /// Registered call sites, each with its observed traffic.
    declared: Vec<DeclaredRow>,
    /// Observed names nobody registered — a call site that shipped without being declared, or a
    /// typo splitting one use case's cost across two rollup keys.
    shadow: Vec<ShadowRow>,
    /// Registered and silent. Whether that is a problem depends on `status`: for an `active` use
    /// case silence is the finding, for a `planned` one it is the expectation.
    quiet: Vec<String>,
    /// Calls that carried no `name` at all, counted separately because in a project that has not
    /// instrumented yet this is the largest and most actionable number on the page — and folding it
    /// into `shadow` would make an uninstrumented project look like a typo problem.
    unattributed_calls: u64,
}

/// The declared-versus-observed report.
pub(crate) async fn coverage(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Query(q): Query<CoverageQuery>,
) -> Result<Json<Coverage>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;

    let store = st.store.clone();
    let (p1, p2) = (pid.clone(), pid.clone());
    let since = q.since.clone();
    let declared_rows = spawn_db(move || store.list_use_cases(&p1)).await?;
    let store = st.store.clone();
    let observed = spawn_db(move || store.observed_use_case_names(&p2, since.as_deref())).await?;

    let find = |key: &str| observed.iter().find(|(n, _, _)| n == key);
    let unattributed_calls = find("").map(|(_, c, _)| *c).unwrap_or(0);

    let mut declared = Vec::with_capacity(declared_rows.len());
    let mut quiet = Vec::new();
    for uc in declared_rows {
        match find(&uc.key) {
            Some((_, calls, models)) => {
                let undeclared_models = models
                    .iter()
                    .filter(|m| uc.declares(m) == Some(false))
                    .cloned()
                    .collect();
                declared.push(DeclaredRow {
                    use_case: uc,
                    calls: *calls,
                    undeclared_models,
                });
            }
            None => {
                quiet.push(uc.key.clone());
                declared.push(DeclaredRow {
                    use_case: uc,
                    calls: 0,
                    undeclared_models: Vec::new(),
                });
            }
        }
    }

    let declared_keys: std::collections::HashSet<&str> =
        declared.iter().map(|d| d.use_case.key.as_str()).collect();
    let shadow = observed
        .iter()
        .filter(|(n, _, _)| !n.is_empty() && !declared_keys.contains(n.as_str()))
        .map(|(n, c, m)| ShadowRow {
            name: n.clone(),
            calls: *c,
            models: m.clone(),
        })
        .collect();

    Ok(Json(Coverage {
        project_id: pid,
        since: q.since,
        declared,
        shadow,
        quiet,
        unattributed_calls,
    }))
}
