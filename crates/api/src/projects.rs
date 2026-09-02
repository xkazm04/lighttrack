//! Project CRUD and archival (admin-only). Their API keys live in [`crate::projects_keys`].

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use lighttrack_core::{new_id, Project, Redaction};

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateProjectReq {
    /// Caller-chosen id. Omit it and the server mints a UUID; supply one and it is validated
    /// ([`validate_project_id`]) and taken verbatim, so the id you then put in `LIGHTTRACK_PROJECT`,
    /// in `/v1/projects/<id>/keys` and in a webhook's `?project=` is the one you chose.
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    redaction: Redaction,
    /// Consent to include this project's benchmark runs in collective digests. Default off.
    #[serde(default)]
    collective_opt_in: bool,
}

/// Longest accepted caller-supplied project id. A project id is a URL path segment, a query value
/// and a document id, so it stays short enough to read in a log line.
const MAX_PROJECT_ID_LEN: usize = 64;

/// Is this a project id we are willing to mint? A project id travels through URL paths
/// (`/v1/projects/<id>/keys`), query strings (`?project=`), env (`LIGHTTRACK_PROJECT`) and — on
/// Firestore — a document path, so the accepted alphabet is the intersection that is unambiguous in
/// all of them: 1–[`MAX_PROJECT_ID_LEN`] characters, first an ASCII letter or digit, then letters,
/// digits, `-`, `_` or `.`. Requiring an alphanumeric first character is what keeps `.`, `..` and
/// `__x__` — reserved or traversal-flavoured document ids — out without a special case each.
///
/// Returns the operator-facing reason on rejection, so a 400 says which rule was broken.
fn validate_project_id(id: &str) -> Result<(), String> {
    let len = id.chars().count();
    if len == 0 || len > MAX_PROJECT_ID_LEN {
        return Err(format!(
            "project id must be 1-{MAX_PROJECT_ID_LEN} characters (got {len})"
        ));
    }
    if !id.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("project id must start with an ASCII letter or digit".to_string());
    }
    if let Some(bad) = id
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(format!(
            "project id may contain only ASCII letters, digits, '-', '_' and '.' (found '{bad}')"
        ));
    }
    Ok(())
}

pub(crate) async fn create_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProjectReq>,
) -> Result<Json<Project>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    // An id the caller chose is honoured, not quietly replaced: it is the string they will type into
    // `LIGHTTRACK_PROJECT` and into the very next documented call (`POST /v1/projects/<id>/keys`),
    // and the dev-mode bootstrap already creates a human-readable `default` project — so readable
    // ids are part of the product, and only the server used to be allowed to have them.
    let id = match &req.id {
        Some(raw) => {
            let id = raw.trim().to_string();
            validate_project_id(&id).map_err(ApiError::bad_request)?;
            // Uniqueness is enforced here rather than left to the store, because the store cannot
            // enforce it uniformly: the SQL backends have a primary key but report the violation as
            // an opaque 500, and Firestore's document write is an upsert that would silently
            // overwrite the existing project. This is a check-then-act, so two admins racing on the
            // same id can both pass it — the loser is then rejected by the SQL primary key, or on
            // Firestore overwrites a project it was itself trying to create. That window is the
            // price of not widening the `Store` trait, and it is bounded to identical intent.
            let store = st.store.clone();
            let probe = id.clone();
            if spawn_db(move || store.get_project(&probe)).await?.is_some() {
                return Err(ApiError::conflict(format!(
                    "project '{id}' already exists — pick another id, or PUT /v1/projects/{id} to \
                     change the existing one"
                )));
            }
            id
        }
        None => new_id(),
    };
    let proj = Project {
        id,
        name: req.name,
        enabled: true,
        redaction: req.redaction,
        collective_opt_in: req.collective_opt_in,
        archived_at: None,
        created_at: Utc::now(),
    };
    insert_project(&st, &proj).await?;
    Ok(Json(proj))
}

/// Write a project row and prime the ingest-path policy cache — the two steps that must always
/// happen together, so a project's persistence policy (hash/drop) is enforced from its very first
/// event rather than from the next cache expiry. Shared by the admin endpoint above and the
/// dev-default bootstrap in [`crate::guards`], so there is one creation path, not two.
pub(crate) async fn insert_project(st: &AppState, proj: &Project) -> Result<(), ApiError> {
    let store = st.store.clone();
    let pc = proj.clone();
    spawn_db(move || store.create_project(&pc)).await?;
    st.project_policies
        .put(&proj.id, crate::state::ProjectPolicy::from(proj));
    Ok(())
}

/// One project by id, or the 404 both this module and [`crate::projects_keys`] answer with. Shared
/// so "unknown project" reads identically whether you were creating a key or updating the tenant.
pub(crate) async fn load_project(st: &AppState, pid: &str) -> Result<Project, ApiError> {
    let store = st.store.clone();
    let id = pid.to_string();
    spawn_db(move || store.get_project(&id))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("project '{pid}' not found")))
}

/// Mutable fields of a project. Every field is optional: an omitted one is left as-is, so a caller
/// tightening `redaction` cannot accidentally reset a name or revoke collective consent.
#[derive(Deserialize)]
pub(crate) struct UpdateProjectReq {
    name: Option<String>,
    enabled: Option<bool>,
    redaction: Option<Redaction>,
    collective_opt_in: Option<bool>,
}

/// Update a project (admin). The reason this endpoint exists at all is `redaction`: it is the one
/// project field that is a *compliance control*, and until now it could only be set at creation —
/// a team that needed to start hashing or dropping payloads had to recreate the project.
///
/// Applying it invalidates the ingest-path policy cache for this project, so the tightened policy is
/// enforced on the **next** event with no restart and no TTL wait.
pub(crate) async fn update_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<UpdateProjectReq>,
) -> Result<Json<Project>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;

    let mut proj = load_project(&st, &pid).await?;
    if let Some(n) = req.name {
        proj.name = n;
    }
    if let Some(e) = req.enabled {
        proj.enabled = e;
    }
    if let Some(r) = req.redaction {
        proj.redaction = r;
    }
    if let Some(c) = req.collective_opt_in {
        proj.collective_opt_in = c;
    }

    let store = st.store.clone();
    let pc = proj.clone();
    let changed = spawn_db(move || store.update_project(&pc)).await?;
    if !changed {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }
    // Invalidate rather than overwrite: the next ingest re-reads the committed row, so the cache can
    // never disagree with what the store actually persisted.
    st.project_policies.invalidate(&proj.id);
    Ok(Json(proj))
}

/// Archive a project (admin): `enabled = false`, `archived_at = now`, **rows kept**.
///
/// `DELETE` here is a lifecycle verb, not a `DROP`. A project's events, scores and benchmark runs
/// are the record every cost report, margin figure and gate decision was computed from; deleting
/// the tenant row would orphan all of it while leaving the numbers in place, so the endpoint that
/// operators reach for does the safe thing and says so. Archiving is also *effective* — a disabled
/// project's keys open nothing (see [`crate::guards`]) — so it stops the spend a delete was meant to.
///
/// Idempotent: archiving an already-archived project returns it unchanged rather than 404-ing.
pub(crate) async fn archive_project(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Project>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let mut proj = load_project(&st, &pid).await?;
    if proj.archived_at.is_none() {
        proj.archived_at = Some(Utc::now());
    }
    proj.enabled = false;

    let store = st.store.clone();
    let pc = proj.clone();
    let changed = spawn_db(move || store.update_project(&pc)).await?;
    if !changed {
        return Err(ApiError::not_found(format!("project '{pid}' not found")));
    }
    st.project_policies.invalidate(&proj.id);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_readable_ids_the_docs_promise() {
        for id in [
            "default", // the id the dev-mode bootstrap itself creates
            "qa-demo",
            "acme_prod",
            "team.eu",
            "p1",
            "A",
            &"a".repeat(MAX_PROJECT_ID_LEN),
        ] {
            assert!(validate_project_id(id).is_ok(), "should accept {id:?}");
        }
    }

    /// A server-minted id must satisfy the rule we impose on callers, or the two id spaces would
    /// diverge the moment anyone round-tripped one.
    #[test]
    fn a_minted_id_passes_the_same_rule() {
        let id = new_id();
        assert!(validate_project_id(&id).is_ok(), "minted {id:?}");
    }

    #[test]
    fn rejects_ids_that_are_unsafe_in_a_url_or_a_document_path() {
        for id in [
            "",                                  // no id at all
            &"a".repeat(MAX_PROJECT_ID_LEN + 1), // too long to read in a log line
            "-leading-dash",                     // must start alphanumeric
            ".",                                 // reserved Firestore document id
            "..",                                // ditto, and path traversal flavoured
            "__proto__",                         // ditto (`__.*__` is reserved)
            "a/b",                               // would forge a second path segment
            "a b",                               // needs escaping in a URL
            "a?b",                               // would start a query string
            "café",                              // non-ASCII: percent-encoding ambiguity
        ] {
            assert!(validate_project_id(id).is_err(), "should reject {id:?}");
        }
    }

    /// The rejection message must name the broken rule — a 400 that only says "invalid" leaves the
    /// operator guessing which of three rules they tripped.
    #[test]
    fn rejection_names_the_rule_that_failed() {
        assert!(validate_project_id("").unwrap_err().contains("1-64"));
        assert!(validate_project_id("-x")
            .unwrap_err()
            .contains("start with"));
        assert!(validate_project_id("a/b").unwrap_err().contains('/'));
    }
}
