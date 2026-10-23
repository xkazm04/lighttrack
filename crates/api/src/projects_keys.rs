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

/// What a rotation does to the predecessor. Two mechanisms, and they are not interchangeable: a
/// stamped deadline is re-evaluated against the clock on every later request, a revocation is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retirement {
    /// Irreversible: the row is disowned and no later clock reading can undo it.
    Revoke,
    /// A stamped absolute deadline. `guards::resolve_principal` compares it against `Utc::now()`
    /// per request, so its meaning is only as true as the clock at both ends.
    ExpireAt(DateTime<Utc>),
}

/// The predecessor's new deadline never *extends* an expiry it already had: rotating a key must
/// only ever shorten its life.
///
/// A window that is already closed is not a bound, and an instant is the wrong instrument for
/// one. `expires_at = now` is re-read against `Utc::now()` on every later request, so a clock
/// that steps backwards — an NTP correction, a host migration, a box that boots without a
/// real-time clock — brings the predecessor back to life; a revocation cannot be undone by any
/// later reading of any clock. So a zero grace window, and a window the predecessor's own expiry
/// has already closed, retire the key outright instead of stamping a date on it.
fn retirement_for(old_expiry: Option<DateTime<Utc>>, grace: i64, now: DateTime<Utc>) -> Retirement {
    if grace <= 0 {
        return Retirement::Revoke;
    }
    let deadline = now + Duration::seconds(grace);
    let deadline = old_expiry.map_or(deadline, |e| e.min(deadline));
    if deadline <= now {
        return Retirement::Revoke;
    }
    Retirement::ExpireAt(deadline)
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

    let retirement = retirement_for(old.expires_at, grace, Utc::now());
    let store = st.store.clone();
    let kid2 = kid.clone();
    let found = match retirement {
        Retirement::Revoke => spawn_db(move || store.set_api_key_revoked(&kid2, true)).await?,
        Retirement::ExpireAt(deadline) => {
            spawn_db(move || store.set_api_key_expiry(&kid2, Some(deadline))).await?
        }
    };
    if !found {
        return Err(ApiError::not_found(format!("key '{kid}' not found")));
    }

    let (revoked, expires_at) = match retirement {
        Retirement::Revoke => (true, old.expires_at),
        Retirement::ExpireAt(deadline) => (false, Some(deadline)),
    };
    Ok(Json(RotateKeyResp {
        successor,
        predecessor: KeyInfo {
            revoked,
            expires_at,
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

    // --- The retirement decision, asked at a clock the code did not read from the system. ---

    const DAY: i64 = 24 * 3600;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Mirrors the authentication window in `guards::resolve_principal`: a key opens doors when it
    /// is not revoked and not yet expired, with `ApiKey::is_expired` reading `expires_at <= now`.
    fn live_at(r: Retirement, at: DateTime<Utc>) -> bool {
        match r {
            Retirement::Revoke => false,
            Retirement::ExpireAt(deadline) => deadline > at,
        }
    }

    /// The three rotations an operator actually performs.
    fn cases(now: DateTime<Utc>) -> [Retirement; 3] {
        [
            // "retire at once"
            retirement_for(None, 0, now),
            // a week for the fleet to redeploy
            retirement_for(None, 7 * DAY, now),
            // a predecessor that already carried an operator-chosen expiry
            retirement_for(Some(t0() + Duration::hours(1)), 7 * DAY, now),
        ]
    }

    /// FLOOR. Every rotation with a real grace window keeps the stamped deadline it has always
    /// had, so nothing about a non-zero rotation may move.
    #[test]
    fn a_non_zero_grace_window_is_stamped_exactly_as_before() {
        assert_eq!(
            retirement_for(None, 7 * DAY, t0()),
            Retirement::ExpireAt(t0() + Duration::seconds(7 * DAY))
        );
        assert_eq!(
            retirement_for(Some(t0() + Duration::hours(1)), 7 * DAY, t0()),
            Retirement::ExpireAt(t0() + Duration::hours(1))
        );
        assert_eq!(
            retirement_for(None, DEFAULT_GRACE_SECS, t0()),
            Retirement::ExpireAt(t0() + Duration::seconds(DEFAULT_GRACE_SECS))
        );
    }

    /// TARGET T1. A rotation is a decision about authority; a backward clock step is a fact of
    /// operational life (an NTP correction, a host migration, a box that boots without a
    /// real-time clock). Count the rotations whose outcome survives one.
    #[test]
    fn a_retirement_survives_a_backward_clock_step() {
        let stepped_back = t0() - Duration::seconds(8 * DAY);
        let survived = cases(t0())
            .into_iter()
            .filter(|r| !live_at(*r, stepped_back))
            .count();
        assert!(
            survived >= 1,
            "no rotation survives a backward clock step: all three predecessors authenticate again"
        );
        // "Retire at once" is the one an operator expects to be irreversible.
        assert!(
            !live_at(cases(t0())[0], stepped_back),
            "a grace window of zero came back to life when the clock stepped back"
        );
    }

    /// TARGET T2. How many of the three outcomes were computed from the issuer's own clock, and
    /// therefore carry whatever error that clock had at stamping time.
    #[test]
    fn count_the_outcomes_that_depend_on_the_stamping_clock() {
        let skewed = t0() - Duration::seconds(8 * DAY);
        let dependent = cases(t0())
            .into_iter()
            .zip(cases(skewed))
            .filter(|(truthful, skewed)| truthful != skewed)
            .count();
        assert!(
            dependent <= 2,
            "every rotation outcome is a function of the stamping clock ({dependent} of 3)"
        );
    }

    /// OBSERVATION, arm-independent: an explicit seven-day request is silently served as one hour
    /// when the predecessor already carried a shorter expiry, and the response says only the
    /// resulting date. Recorded because the clamp is correct and its silence is the defect.
    #[test]
    fn an_explicit_grace_request_is_clamped_without_a_word() {
        let asked = 7 * DAY;
        let got = match retirement_for(Some(t0() + Duration::hours(1)), asked, t0()) {
            Retirement::ExpireAt(d) => d - t0(),
            Retirement::Revoke => Duration::zero(),
        };
        assert_eq!(got, Duration::hours(1));
        assert!(got < Duration::seconds(asked));
    }
}
