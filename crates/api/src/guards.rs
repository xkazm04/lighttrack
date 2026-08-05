//! Request authentication + project-scoping guards.

use axum::http::HeaderMap;
use chrono::Utc;

use crate::auth::{self, AuthMode, Principal};
use crate::auth_throttle;
use crate::error::{ApiError, ErrorCode};
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
///
/// This is the one place a bearer token is accepted or rejected, so it is also where **failed**
/// attempts are metered (see [`auth_throttle`]). The throttle is consulted *before* the credential is
/// compared — a check that ran afterwards would relabel the response without slowing the guessing —
/// and only a genuine 401 counts against a source, never a store outage.
pub(crate) async fn authenticate(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<Principal, ApiError> {
    auth_throttle::guard(st)?;
    match resolve_principal(st, headers).await {
        Ok(p) => {
            auth_throttle::record_success(st);
            Ok(p)
        }
        Err(e) => {
            if e.code() == ErrorCode::Unauthorized {
                auth_throttle::record_failure(st);
            }
            Err(e)
        }
    }
}

async fn resolve_principal(st: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
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
                return Ok(Principal::Project {
                    project_id: k.project_id,
                    key_id: k.id,
                });
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

/// Where a keyless dev-mode caller's events land when they name no project. Dev mode only — see
/// [`resolve_ingest_project`].
pub(crate) const DEV_DEFAULT_PROJECT: &str = "default";

/// The 400 for an ingest that cannot be attributed to any project. Names both fixes, because the
/// failure is silent from the caller's side (the SDKs buffer and swallow it) and "project_id is
/// required" told nobody *how* to supply one.
pub(crate) const NO_PROJECT_MSG: &str =
    "project_id is required: set it on the event (SDKs read LIGHTTRACK_PROJECT), or present a \
     project API key — POST /v1/projects then POST /v1/projects/:id/keys — which derives the \
     project server-side";

/// Which project an ingested event belongs to. A project key forces its own project.
pub(crate) fn resolve_ingest_project(
    p: &Principal,
    body_project: &str,
) -> Result<String, ApiError> {
    match p {
        Principal::Project { project_id, .. } => Ok(project_id.clone()),
        // The zero-config first run: dev mode, no key, no project named. Rejecting it with a 400 is
        // what made the documented quickstart fail *silently* — the SDKs swallow the error, so the
        // user sees no events and no reason. Attribute it to a real default project instead.
        //
        // This cannot widen enforced-mode behaviour: `authenticate` only ever produces
        // `Principal::Dev` under `AuthMode::Dev` (enforced mode 401s a missing/unknown token long
        // before here), and an `Admin` principal — the one identity that exists in both modes —
        // still falls through to the 400 below.
        Principal::Dev if body_project.trim().is_empty() => Ok(DEV_DEFAULT_PROJECT.to_string()),
        Principal::Admin | Principal::Dev => {
            if body_project.trim().is_empty() {
                Err(ApiError::bad_request(NO_PROJECT_MSG))
            } else {
                Ok(body_project.to_string())
            }
        }
    }
}

/// [`resolve_ingest_project`] for the event front doors, plus the create-if-missing that makes the
/// dev default a *real* row in the project registry.
///
/// The store read costs one extra query, but only on the path that took the dev default (dev mode,
/// no key, no project named) — keyed and explicitly-projected ingest never reaches it.
pub(crate) async fn resolve_ingest_project_ensuring(
    st: &AppState,
    p: &Principal,
    body_project: &str,
) -> Result<String, ApiError> {
    let pid = resolve_ingest_project(p, body_project)?;
    if matches!(p, Principal::Dev) && body_project.trim().is_empty() {
        ensure_dev_default_project(st).await;
    }
    Ok(pid)
}

/// Create [`DEV_DEFAULT_PROJECT`] if it isn't there yet, and say so once — at `info`, so an operator
/// who named no project can find where their events went.
///
/// Best-effort by design: no ingest step depends on the row existing (an event carries its own
/// `project_id` and there is no foreign key), so a failure here — including losing the create race
/// to a concurrent first event — must never turn a good event into an error. The row exists so the
/// project shows up in `GET /v1/projects`, can be given limits, and can be opened in the UI.
async fn ensure_dev_default_project(st: &AppState) {
    let store = st.store.clone();
    match spawn_db(move || store.get_project(DEV_DEFAULT_PROJECT)).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(project_id = DEV_DEFAULT_PROJECT, error = %e, "could not look up the dev default project");
            return;
        }
    }
    let proj = lighttrack_core::Project {
        id: DEV_DEFAULT_PROJECT.to_string(),
        name: DEV_DEFAULT_PROJECT.to_string(),
        enabled: true,
        redaction: lighttrack_core::Redaction::None,
        collective_opt_in: false,
        created_at: Utc::now(),
    };
    match crate::projects::insert_project(st, &proj).await {
        Ok(()) => tracing::info!(
            project_id = DEV_DEFAULT_PROJECT,
            "an event arrived with no project_id and no API key: created project \
             '{DEV_DEFAULT_PROJECT}' and attributed it there. Set project_id on the event (SDKs \
             read LIGHTTRACK_PROJECT) or mint a project key to send it elsewhere. Dev mode only — \
             under LIGHTTRACK_AUTH_MODE=enforced this request would be a 400."
        ),
        Err(e) => {
            tracing::warn!(project_id = DEV_DEFAULT_PROJECT, error = %e, "could not create the dev default project")
        }
    }
}

/// Which project a read may target. A project key may only read its own project.
pub(crate) fn resolve_read_project(
    p: &Principal,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    match p {
        Principal::Project {
            project_id: pid, ..
        } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn project_principal() -> Principal {
        Principal::Project {
            project_id: "proj-a".into(),
            key_id: "key-1".into(),
        }
    }

    /// `ApiError` is deliberately not `Debug` (it is a wire envelope, not a diagnostic), so flatten
    /// it to its `Display` form for assertions.
    fn resolved(p: &Principal, body: &str) -> Result<String, String> {
        resolve_ingest_project(p, body).map_err(|e| e.to_string())
    }

    #[test]
    fn dev_principal_falls_back_to_the_default_project_but_admin_never_does() {
        // The dev fallback exists so the documented first run works; it is reachable ONLY through
        // `Principal::Dev`, which `authenticate` produces only under `AuthMode::Dev`.
        assert_eq!(resolved(&Principal::Dev, "").unwrap(), DEV_DEFAULT_PROJECT);
        assert_eq!(
            resolved(&Principal::Dev, "   ").unwrap(),
            DEV_DEFAULT_PROJECT
        );
        // A named project still wins — the default never overrides an explicit one.
        assert_eq!(resolved(&Principal::Dev, "mine").unwrap(), "mine");

        // Admin is the one principal that exists in BOTH modes, so it must keep refusing: this is
        // what stops the dev convenience from leaking into an enforced deployment.
        assert_eq!(
            resolved(&Principal::Admin, "").unwrap_err(),
            format!("bad_request: {NO_PROJECT_MSG}")
        );
        assert!(resolved(&Principal::Admin, " ").is_err());
        assert_eq!(resolved(&Principal::Admin, "mine").unwrap(), "mine");
    }

    #[test]
    fn a_project_key_forces_its_own_project() {
        // Unchanged by the dev fallback: a key ignores the body, blank or not.
        assert_eq!(resolved(&project_principal(), "").unwrap(), "proj-a");
        assert_eq!(resolved(&project_principal(), "proj-b").unwrap(), "proj-a");
    }
}
