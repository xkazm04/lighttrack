//! Request authentication + project-scoping guards.

use axum::http::HeaderMap;
use chrono::Utc;

use crate::auth::{self, AuthMode, Principal};
use crate::error::ApiError;
use crate::state::{spawn_db, AppState};

pub(crate) fn bearer(headers: &HeaderMap) -> Option<String> {
    let h = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let rest = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))?;
    Some(rest.trim().to_string())
}

/// Resolve the principal behind a request (see `auth` module for mode semantics).
pub(crate) async fn authenticate(st: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let token = match bearer(headers) {
        Some(t) => t,
        None => {
            return match st.auth_mode {
                AuthMode::Dev => Ok(Principal::Dev),
                AuthMode::Enforced => Err(ApiError::unauthorized("missing API key")),
            }
        }
    };

    // Constant-time: this is the one credential compared against a raw secret rather than a stored
    // digest, and it is the highest-privilege one on the instance.
    if let Some(admin) = &st.admin_key {
        if auth::secret_eq(&token, admin) {
            return Ok(Principal::Admin);
        }
    }

    if let Some(prefix) = auth::prefix_of(&token) {
        let store = st.store.clone();
        let key = spawn_db(move || store.find_api_key_by_prefix(&prefix)).await?;
        if let Some(k) = key {
            if !k.revoked && auth::verify_key(&k.key_hash, &token) {
                // Best-effort, detached: record last use without delaying the request.
                let store2 = st.store.clone();
                let id = k.id.clone();
                tokio::spawn(async move {
                    let _ =
                        tokio::task::spawn_blocking(move || store2.touch_api_key(&id, Utc::now()))
                            .await;
                });
                return Ok(Principal::Project { project_id: k.project_id, key_id: k.id });
            }
        }
    }

    match st.auth_mode {
        AuthMode::Dev => Ok(Principal::Dev), // lenient in dev: ignore an unrecognized token
        AuthMode::Enforced => Err(ApiError::unauthorized("invalid API key")),
    }
}

pub(crate) fn ensure_can_admin(p: &Principal) -> Result<(), ApiError> {
    match p {
        Principal::Admin | Principal::Dev => Ok(()),
        Principal::Project { .. } => Err(ApiError::forbidden("admin privileges required")),
    }
}

/// Which project an ingested event belongs to. A project key forces its own project.
pub(crate) fn resolve_ingest_project(p: &Principal, body_project: &str) -> Result<String, ApiError> {
    match p {
        Principal::Project { project_id, .. } => Ok(project_id.clone()),
        Principal::Admin | Principal::Dev => {
            if body_project.trim().is_empty() {
                Err(ApiError::bad_request("project_id is required"))
            } else {
                Ok(body_project.to_string())
            }
        }
    }
}

/// Which project a read may target. A project key may only read its own project.
pub(crate) fn resolve_read_project(
    p: &Principal,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    match p {
        Principal::Project { project_id: pid, .. } => {
            if let Some(r) = requested {
                if r != pid {
                    return Err(ApiError::forbidden("key not authorized for that project"));
                }
            }
            Ok(Some(pid.clone()))
        }
        Principal::Admin | Principal::Dev => Ok(requested.map(str::to_string)),
    }
}
