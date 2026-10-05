//! One router layer where the contract's `access` column becomes enforcement.
//!
//! `crates/contract` exists to end hand-kept route lists, and it succeeded for OpenAPI, the MCP
//! catalog, the CLI and the renderer — each is *generated* from `endpoints()`. Authorization was
//! the one consumer left as a *pin*: a test read `include_str!("main.rs")`, split it on `"/v1`, and
//! compared the resulting path strings against `route_paths()`, which `dedup()`s. Method was
//! invisible, prose about a route satisfied it, and nothing asserted that an `access: Admin` row
//! refuses a project key.
//!
//! This layer resolves (`Method`, [`MatchedPath`]) — axum hands back the route template verbatim,
//! which is the very string the table stores — to one `Endpoint`, and applies its declared access
//! **before any extractor runs**. That ordering is half the point: `Json<T>` used to be extracted
//! first, so an unauthenticated POST learned the body schema from a 422 before being told it had no
//! business there.
//!
//! The handler-side `authenticate` / `ensure_can_admin` / `ensure_scope` calls stay in place: this
//! is belt-and-braces by design, so the layer can be reverted by deleting one `.layer(..)` line and
//! so a later series can remove the repeated guard calls against a proof that already exists.

use axum::extract::{MatchedPath, Request, State};
use axum::http::Method as HttpMethod;
use axum::middleware::Next;
use axum::response::Response;

use lighttrack_contract::{Access, Method};
use lighttrack_core::Scope;

use crate::auth_scopes::ensure_scope;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::AppState;

/// The contract speaks four verbs. `HEAD` resolves as the `GET` it is served by — axum answers a
/// `HEAD` from the `GET` handler, and a probe that skipped authorization would be a hole. Anything
/// else (`OPTIONS`, `PATCH`, …) is outside the table's vocabulary, so the layer declines to invent a
/// verdict and lets the `MethodRouter` answer `405`.
fn contract_method(m: &HttpMethod) -> Option<Method> {
    if m == HttpMethod::GET || m == HttpMethod::HEAD {
        Some(Method::Get)
    } else if m == HttpMethod::POST {
        Some(Method::Post)
    } else if m == HttpMethod::PUT {
        Some(Method::Put)
    } else if m == HttpMethod::DELETE {
        Some(Method::Delete)
    } else {
        None
    }
}

/// [`authenticate`], bounded by the server deadline.
///
/// `shed::ingest_admission` wraps the handler in that deadline, and before this layer existed the
/// handler was where authentication happened — so a blocking pool starved by a slow store answered
/// `504 timeout`. A `Router::layer` cannot be placed inside a route's own layers, so the layer runs
/// *outside* that wrapper and has to carry the deadline itself; without it a starved pool would
/// hang the request forever, which is strictly worse than the honest cut.
async fn authenticate_within_deadline(
    st: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<crate::auth::Principal, ApiError> {
    match st.ingest_guard.deadline() {
        None => authenticate(st, headers).await,
        Some(d) => match tokio::time::timeout(d, authenticate(st, headers)).await {
            Ok(r) => r,
            Err(_) => {
                st.ingest_guard.record_timeout();
                Err(ApiError::new(
                    crate::error::ErrorCode::Timeout,
                    format!(
                        "authentication exceeded the {}s server deadline; the store did not answer \
                         in time — retry, and check /v1/storage/status",
                        d.as_secs()
                    ),
                ))
            }
        },
    }
}

/// Is the presented bearer a *device* credential by shape — an enrolled `ltd_…` key, or the
/// deprecated shared one? This answers "which authority owns this comparison", never "is it valid":
/// a revoked or forged device key still reaches `ensure_device`, which refuses it with the error
/// that names the real problem.
fn presents_device_credential(st: &AppState, headers: &axum::http::HeaderMap) -> bool {
    match crate::guards::bearer(headers) {
        Some(token) => {
            crate::auth::device_prefix_of(&token).is_some()
                || st
                    .relay_device_key
                    .as_deref()
                    .is_some_and(|expected| crate::auth::secret_eq(&token, expected))
        }
        None => false,
    }
}

pub(crate) async fn enforce(
    State(st): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let template = match req.extensions().get::<MatchedPath>() {
        Some(p) => p.as_str().to_string(),
        // Routing placed this request on no route (the fallback). There is no row to consult, and
        // the 404 is the router's to give.
        None => return Ok(next.run(req).await),
    };
    let Some(method) = contract_method(req.method()) else {
        return Ok(next.run(req).await);
    };

    let row =
        match lighttrack_contract::endpoints().find(|e| e.path == template && e.method == method) {
            Some(e) => e,
            None if lighttrack_contract::endpoints().any(|e| e.path == template) => {
                // The path is declared, this verb is not. That is a `405`, which the `MethodRouter`
                // below states better than this layer could.
                return Ok(next.run(req).await);
            }
            None => {
                // A route the table does not declare is a bug, not a pass: nobody decided who may call
                // it. Loud, named and attributable — `/health/live` and `/health/ready` are the two
                // this rule caught, and they are declared now rather than exempted.
                return Err(ApiError::internal(format!(
                    "{} {template} is wired in build_router but declares no Endpoint row in \
                 crates/contract — who may call it has not been decided",
                    method.as_str(),
                )));
            }
        };

    match row.access {
        // `/health`, the probes, `/openapi.json`, and the HMAC-signed billing webhook, which
        // authenticates itself against the provider's secret rather than a LightTrack principal.
        Access::Unauthenticated => return Ok(next.run(req).await),
        // A `machine: true` row is "a door only a program walks through", and some of those
        // programs are not LightTrack principals at all: a relay device presents an enrolled
        // `ltd_…` key (or the deprecated shared one), which `authenticate` cannot resolve and must
        // not be asked to. When such a credential is actually presented, the layer steps aside and
        // `relay_devices::ensure_device` — the authority on that class, which the handler runs
        // regardless — decides. No credential still gets the 401 below, and a *project* key still
        // gets the 403: deferring is not exempting.
        Access::Admin if row.machine && presents_device_credential(&st, req.headers()) => {
            return Ok(next.run(req).await)
        }
        Access::Admin => {
            let p = authenticate_within_deadline(&st, req.headers()).await?;
            ensure_can_admin(&p)?;
            req.extensions_mut().insert(p);
        }
        Access::Key(k) => {
            // The contract states scopes as strings so it can stay dependency-free;
            // `auth_scopes::every_declared_key_scope_maps_onto_a_real_scope` holds the two sides
            // together, which is why an unmappable one here is an internal error and not a refusal.
            let want = Scope::ALL
                .into_iter()
                .find(|s| s.as_str() == k.as_str())
                .ok_or_else(|| {
                    ApiError::internal(format!(
                        "{}: contract scope '{}' maps onto no Scope",
                        row.id,
                        k.as_str()
                    ))
                })?;
            let p = authenticate_within_deadline(&st, req.headers()).await?;
            ensure_scope(&p, want)?;
            req.extensions_mut().insert(p);
        }
    }

    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_resolves_as_the_get_it_is_served_by() {
        assert_eq!(contract_method(&HttpMethod::HEAD), Some(Method::Get));
        assert_eq!(contract_method(&HttpMethod::GET), Some(Method::Get));
        assert_eq!(contract_method(&HttpMethod::DELETE), Some(Method::Delete));
    }

    /// A verb the contract cannot express must fall through to the router's own `405`, never be
    /// resolved against a row that happens to share the path.
    #[test]
    fn a_verb_outside_the_contract_is_not_resolved() {
        assert_eq!(contract_method(&HttpMethod::OPTIONS), None);
        assert_eq!(contract_method(&HttpMethod::PATCH), None);
    }
}
