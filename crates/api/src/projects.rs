//! Projects & API keys management (admin-only).

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, ApiKey, Project, Redaction};

use crate::auth;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateProjectReq {
    name: String,
    #[serde(default)]
    redaction: Redaction,
}

pub(crate) async fn create_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProjectReq>,
) -> Result<Json<Project>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let proj = Project {
        id: new_id(),
        name: req.name,
        enabled: true,
        redaction: req.redaction,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let pc = proj.clone();
    spawn_db(move || store.create_project(&pc)).await?;
    // Keep the ingest-path policy cache current, so the new project's persistence policy (hash/drop)
    // is enforced from its very first event.
    st.redaction_policies.write().unwrap().insert(proj.id.clone(), proj.redaction);
    Ok(Json(proj))
}

pub(crate) async fn list_projects(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Project>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_projects()).await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(crate) struct CreateKeyReq {
    #[serde(default = "default_key_name")]
    name: String,
}

fn default_key_name() -> String {
    "default".to_string()
}

#[derive(Serialize)]
pub(crate) struct CreateKeyResp {
    id: String,
    project_id: String,
    name: String,
    prefix: String,
    /// The full secret — shown exactly once.
    key: String,
    created_at: DateTime<Utc>,
}

pub(crate) async fn create_key(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<CreateKeyResp>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let store = st.store.clone();
    let pid_check = pid.clone();
    if spawn_db(move || store.get_project(&pid_check)).await?.is_none() {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }

    let generated = auth::generate_key();
    let now = Utc::now();
    let key = ApiKey {
        id: new_id(),
        project_id: pid.clone(),
        name: req.name,
        prefix: generated.prefix.clone(),
        key_hash: generated.key_hash,
        created_at: now,
        last_used_at: None,
        revoked: false,
    };

    let store = st.store.clone();
    let key2 = key.clone();
    spawn_db(move || store.create_api_key(&key2)).await?;

    Ok(Json(CreateKeyResp {
        id: key.id,
        project_id: pid,
        name: key.name,
        prefix: generated.prefix,
        key: generated.full_key,
        created_at: now,
    }))
}
