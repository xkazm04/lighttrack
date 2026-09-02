//! What this deployment's store backend can actually do, published rather than discovered.
//!
//! Before the manifest, the only way for an operator (or an SDK) to learn that a surface was missing
//! was to call it and read a 501 — after building a feature on it. `GET /v1/capabilities` turns that
//! into something you can check up front, and the startup log names every gap once, at boot, on the
//! backend the deployment actually chose.

use axum::{extract::State, http::HeaderMap, Json};
use lighttrack_store::{Capabilities, Surface};
use serde::Serialize;
use serde_json::json;

use crate::error::ApiError;
use crate::guards::authenticate;
use crate::state::AppState;

/// The manifest as served. `surfaces` is what works; `unsupported` is what answers 501, named
/// explicitly rather than left to be inferred from the first list's absences — a client that has to
/// diff two lists to find out what is missing will not do it.
#[derive(Debug, Serialize)]
pub(crate) struct CapabilitiesBody {
    pub backend: &'static str,
    pub surfaces: Vec<&'static str>,
    pub unsupported: Vec<&'static str>,
    /// Whether a configured usage cap is genuinely enforced under concurrent ingest. `false` means
    /// caps here are **advisory**: a burst can exceed one before it takes effect.
    pub atomic_admission: bool,
}

impl From<Capabilities> for CapabilitiesBody {
    fn from(c: Capabilities) -> Self {
        Self {
            backend: c.backend,
            surfaces: c.surfaces.iter().map(|s| s.as_str()).collect(),
            unsupported: c.missing().iter().map(|s| s.as_str()).collect(),
            atomic_admission: c.atomic_admission,
        }
    }
}

/// `GET /v1/capabilities` — any authenticated principal.
///
/// Not admin-gated: it names no data and no configuration, only which routes this deployment can
/// answer. An SDK deciding whether to offer trace views, and a project owner wondering why
/// `/v1/forecast` 501s, both need it — gating it behind the admin key would push them back to
/// probing endpoints to find out.
pub(crate) async fn get_capabilities(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CapabilitiesBody>, ApiError> {
    let principal = authenticate(&st, &headers).await?;
    // Declared as a read in ROUTE_SCOPES: the manifest names surfaces, never data, but a key that
    // may not read anything has no reason to enumerate what it could not reach.
    crate::auth_scopes::ensure_scope(&principal, lighttrack_core::Scope::Read)?;
    Ok(Json(st.store.capabilities().into()))
}

/// `GET /health` — liveness, plus what this deployment's store can serve.
///
/// Unauthenticated (a liveness probe is not a credential) and it names no data, only which surfaces
/// exist. Folding the manifest in here means the answer to "why does /v1/forecast 501 in prod" is in
/// the one endpoint every operator already curls. `status` stays first and stays `"ok"` — the smoke
/// script and the container healthcheck read it.
pub(crate) async fn health(State(st): State<AppState>) -> Json<serde_json::Value> {
    let caps = st.store.capabilities();
    Json(json!({
        "status": "ok",
        "backend": caps.backend,
        "capabilities": CapabilitiesBody::from(caps),
    }))
}

/// Log the backend's declaration at startup: one line for the manifest, then one `warn!` per
/// undeclared surface.
///
/// Per-surface and at `warn`, deliberately. A single line listing six missing surfaces is skimmed;
/// six lines each naming one surface and what it costs are searchable, and an operator who
/// configured a cap on Firestore should not learn that it is advisory from a support ticket.
pub(crate) fn log_posture(caps: &Capabilities) {
    tracing::info!(
        backend = caps.backend,
        surfaces = caps.surfaces.len(),
        atomic_admission = caps.atomic_admission,
        "store capability manifest (see docs/PARITY.md)"
    );
    if !caps.atomic_admission {
        tracing::warn!(
            backend = caps.backend,
            "usage caps are ADVISORY on this backend — admission is check-then-act, so a \
             concurrent burst can exceed a cap before it takes effect. Postgres \
             (LIGHTTRACK_DATABASE_URL=postgres://…) enforces caps atomically."
        );
    }
    for s in caps.missing() {
        tracing::warn!(
            backend = caps.backend,
            surface = s.as_str(),
            "this surface is NOT served on this backend — its routes answer HTTP 501 \
             `unsupported` rather than an empty result. {}",
            consequence(s)
        );
    }
}

/// What an operator actually loses, per surface — the sentence that makes the warning actionable.
fn consequence(s: Surface) -> &'static str {
    match s {
        Surface::EventsCore | Surface::EventFilters => {
            "This is the ingest/read floor; a backend missing it cannot serve the API at all."
        }
        Surface::RedactionPosture => {
            "GET /v1/projects/:id/redaction is unavailable — this deployment cannot say whether its              stored rows were scrubbed, or by which rule set."
        }
        Surface::Traces => "/v1/traces, /v1/traces/:id and whole-trace scoring are unavailable.",
        Surface::Forecast => "/v1/forecast and the pre-emptive breach alerts are unavailable.",
        Surface::MarginBreakdowns => {
            "The per-customer margin drill-down and pricing what-if are unavailable."
        }
        Surface::Prompts => "The prompt registry (/v1/projects/:id/prompts) is unavailable.",
        Surface::Relay => "The device relay queue (/v1/relay/*) is unavailable.",
        Surface::Collective => "The collective leaderboard hub cannot store contributions here.",
        Surface::ProjectAdmin => {
            "PUT /v1/projects/:id is unavailable — a project's redaction policy cannot be changed."
        }
        Surface::KeyAdmin => "Listing and revoking a project's API keys is unavailable.",
        Surface::LimitLifecycle => {
            "A limit rule cannot be read, updated or deleted after it is created."
        }
        Surface::JobLeases => "Jobs cannot be cancelled and leases cannot be renewed.",
        Surface::Maintenance => {
            "Disk accounting and the maintenance sweep are the managed service's job here."
        }
        Surface::Metrics => "The store reports no latency profile of its own.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_names_both_halves() {
        let caps = Capabilities::new("test", &[Surface::EventsCore, Surface::Traces], true);
        let body = CapabilitiesBody::from(caps);
        assert_eq!(body.backend, "test");
        assert!(body.surfaces.contains(&"traces"));
        assert!(
            body.unsupported.contains(&"relay"),
            "the gaps are named, not left to be inferred: {:?}",
            body.unsupported
        );
        assert_eq!(
            body.surfaces.len() + body.unsupported.len(),
            Surface::ALL.len(),
            "every surface appears in exactly one of the two lists"
        );
    }

    /// Every surface has a sentence saying what its absence costs; a fallback would let a new
    /// surface ship with a warning that tells the operator nothing.
    #[test]
    fn every_surface_has_a_consequence() {
        for &s in Surface::ALL {
            assert!(!consequence(s).is_empty(), "{s:?}");
        }
    }
}
