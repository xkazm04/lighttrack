//! API keys on a project: mint, list, revoke, rotate (admin-only).
//!
//! Split out of [`crate::projects`], which had grown past the file budget while owning two
//! different lifecycles — the tenant's and its credentials'.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::{default_scopes, new_id, ApiKey, Scope};

use crate::auth;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::projects::load_project;
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateKeyReq {
    #[serde(default = "default_key_name")]
    name: String,
    /// What the key may do. Omitted ⇒ the permissive back-compat default (`ingest` + `read`); the
    /// documented next default is `["ingest"]`, so a key that only ships telemetry should say so.
    #[serde(default)]
    scopes: Option<Vec<String>>,
    /// Optional hard expiry (RFC3339). Past it the key authenticates as nothing (401 `key_expired`).
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

fn default_key_name() -> String {
    "default".to_string()
}

/// Parse the requested scopes, rejecting an unknown one rather than silently dropping it — a
/// typo'd `"reed"` that quietly minted a key with no read access is a support ticket, not a key.
fn parse_scopes(raw: &Option<Vec<String>>) -> Result<Vec<Scope>, ApiError> {
    let Some(raw) = raw else {
        return Ok(default_scopes());
    };
    if raw.is_empty() {
        return Err(ApiError::bad_request(
            "scopes must name at least one of ingest, read, manage — a key with none opens nothing",
        ));
    }
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let parsed = Scope::parse(s).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unknown scope '{s}': expected one of ingest, read, manage"
            ))
        })?;
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    Ok(out)
}

#[derive(Serialize)]
pub(crate) struct CreateKeyResp {
    id: String,
    project_id: String,
    name: String,
    prefix: String,
    /// The full secret — shown exactly once.
    key: String,
    scopes: Vec<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

/// Mint a key on a project, with the requested capabilities and optional expiry.
async fn mint(
    st: &AppState,
    pid: &str,
    name: String,
    scopes: Vec<Scope>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<CreateKeyResp, ApiError> {
    let generated = auth::generate_key();
    let now = Utc::now();
    let key = ApiKey {
        id: new_id(),
        project_id: pid.to_string(),
        name,
        prefix: generated.prefix.clone(),
        key_hash: generated.key_hash,
        created_at: now,
        last_used_at: None,
        revoked: false,
        scopes,
        expires_at,
    };
    let store = st.store.clone();
    let key2 = key.clone();
    spawn_db(move || store.create_api_key(&key2)).await?;
    Ok(CreateKeyResp {
        id: key.id,
        project_id: key.project_id,
        name: key.name,
        prefix: generated.prefix,
        key: generated.full_key,
        scopes: key.scopes,
        expires_at: key.expires_at,
        created_at: now,
    })
}

pub(crate) async fn create_key(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateKeyReq>,
) -> Result<Json<CreateKeyResp>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    load_project(&st, &pid).await?;
    let scopes = parse_scopes(&req.scopes)?;
    if let Some(exp) = req.expires_at {
        if exp <= Utc::now() {
            return Err(ApiError::bad_request(
                "expires_at is already in the past — the key would be dead on arrival",
            ));
        }
    }
    Ok(Json(
        mint(&st, &pid, req.name, scopes, req.expires_at).await?,
    ))
}

/// A key's non-secret metadata — everything an operator needs to audit and rotate, and **never**
/// `key_hash`. (A bare `ApiKey` derives `Serialize` over the hash, so we project into this instead.)
#[derive(Serialize)]
pub(crate) struct KeyInfo {
    id: String,
    name: String,
    prefix: String,
    created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<DateTime<Utc>>,
    revoked: bool,
    scopes: Vec<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
}

impl KeyInfo {
    fn of(k: ApiKey) -> Self {
        Self {
            id: k.id,
            name: k.name,
            prefix: k.prefix,
            created_at: k.created_at,
            last_used_at: k.last_used_at,
            revoked: k.revoked,
            scopes: k.scopes,
            expires_at: k.expires_at,
        }
    }
}

/// List a project's API keys (admin). Surfaces `last_used_at`, `revoked`, `scopes` and `expires_at`
/// so an operator can spot a stale key, see what it can do, and confirm a rotation drained the old one.
pub(crate) async fn list_keys(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<KeyInfo>>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    load_project(&st, &pid).await?;
    let store = st.store.clone();
    let keys = spawn_db(move || store.list_api_keys(&pid)).await?;
    Ok(Json(keys.into_iter().map(KeyInfo::of).collect()))
}

/// One key of a project, by id. Scoped to the path project so an admin can't reach across tenants
/// by id-guessing beyond the projects they can already see.
async fn load_key(st: &AppState, pid: &str, kid: &str) -> Result<ApiKey, ApiError> {
    let store = st.store.clone();
    let owner = pid.to_string();
    let keys = spawn_db(move || store.list_api_keys(&owner)).await?;
    keys.into_iter()
        .find(|k| k.id == kid)
        .ok_or_else(|| ApiError::not_found(format!("key '{kid}' not found on project '{pid}'")))
}

/// Revoke an API key (admin, soft — the row is kept for audit). Revocation is immediate: auth reads
/// the store per request and rejects a revoked key, so a leaked key is dead on the next call.
pub(crate) async fn revoke_key(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, kid)): Path<(String, String)>,
) -> Result<Json<KeyInfo>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let key = load_key(&st, &pid, &kid).await?;
    let store = st.store.clone();
    let kid2 = kid.clone();
    spawn_db(move || store.set_api_key_revoked(&kid2, true)).await?;
    Ok(Json(KeyInfo {
        revoked: true,
        ..KeyInfo::of(key)
    }))
}

/// How long a rotated key keeps working by default: long enough to redeploy every process holding
/// it, short enough that a forgotten rotation is not a permanent second live credential.
const DEFAULT_GRACE_SECS: i64 = 3600;
/// A grace window longer than this is almost certainly a units mistake (days typed as seconds), and
/// a leaked predecessor living for months is the thing rotation exists to prevent.
const MAX_GRACE_SECS: i64 = 7 * 24 * 3600;

#[derive(Deserialize)]
pub(crate) struct RotateKeyReq {
    /// How long the predecessor keeps working. `0` retires it immediately.
    #[serde(default)]
    grace_secs: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct RotateKeyResp {
    /// The successor — same name and scopes, new secret, shown exactly once.
    successor: CreateKeyResp,
    /// The predecessor, now carrying the expiry that closes the grace window.
    predecessor: KeyInfo,
}

/// Rotate a key: mint a successor with the same name and scopes, and give the predecessor a
/// deadline instead of killing it outright — so a fleet still holding the old secret has a window
/// to redeploy rather than a cliff.
///
/// The window is a **stamped `expires_at`**, not a scheduled revoke: a background task would be
/// lost on the next restart, and would then leave the old key live forever — the exact failure
/// rotation exists to prevent. A `grace_secs` of `0` retires the predecessor at once.
pub(crate) async fn rotate_key(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, kid)): Path<(String, String)>,
    Json(req): Json<RotateKeyReq>,
) -> Result<Json<RotateKeyResp>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let old = load_key(&st, &pid, &kid).await?;
    if old.revoked {
        return Err(ApiError::conflict(format!(
            "key '{kid}' is revoked; there is nothing to rotate — mint a new one with \
             POST /v1/projects/{pid}/keys"
        )));
    }
    let grace = req.grace_secs.unwrap_or(DEFAULT_GRACE_SECS);
    if !(0..=MAX_GRACE_SECS).contains(&grace) {
        return Err(ApiError::bad_request(format!(
            "grace_secs must be between 0 and {MAX_GRACE_SECS} (got {grace})"
        )));
    }

    let successor = mint(
        &st,
        &pid,
        old.name.clone(),
        old.scopes.clone(),
        old.expires_at,
    )
    .await?;

    // The predecessor's new deadline never *extends* an expiry it already had: rotating a key must
    // only ever shorten its life.
    let deadline = Utc::now() + Duration::seconds(grace);
    let deadline = old.expires_at.map_or(deadline, |e| e.min(deadline));
    let store = st.store.clone();
    let kid2 = kid.clone();
    if !spawn_db(move || store.set_api_key_expiry(&kid2, Some(deadline))).await? {
        return Err(ApiError::not_found(format!("key '{kid}' not found")));
    }

    Ok(Json(RotateKeyResp {
        successor,
        predecessor: KeyInfo {
            expires_at: Some(deadline),
            ..KeyInfo::of(old)
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(v: &[&str]) -> Result<Vec<Scope>, String> {
        parse_scopes(&Some(v.iter().map(|s| s.to_string()).collect())).map_err(|e| e.to_string())
    }

    #[test]
    fn omitted_scopes_take_the_back_compat_default() {
        assert_eq!(
            parse_scopes(&None).map_err(|e| e.to_string()),
            Ok(default_scopes())
        );
    }

    /// A typo must be a 400, not a key that silently opens fewer doors than the operator asked for.
    #[test]
    fn an_unknown_scope_is_refused_by_name() {
        let err = parsed(&["ingest", "reed"]).unwrap_err();
        assert!(err.contains("unknown scope 'reed'"), "{err}");
        assert!(parsed(&[]).unwrap_err().contains("at least one"));
    }

    #[test]
    fn scopes_are_parsed_case_insensitively_and_deduplicated() {
        assert_eq!(
            parsed(&["INGEST", "ingest", "Manage"]),
            Ok(vec![Scope::Ingest, Scope::Manage])
        );
    }
}
