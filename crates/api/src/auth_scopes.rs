//! What a project key is allowed to do, per route.
//!
//! Three capabilities on a key — `ingest`, `read`, `manage` — and deliberately **not** RBAC: there
//! are no roles, no inheritance, and no per-resource grants (the non-goal in `docs/DECISIONS.md`
//! stands). The point is narrower and concrete: an ingest key embedded in a shipped client app used
//! to be able to read every stored prompt and completion in its project, because there was exactly
//! one shape of project principal.
//!
//! Enforcement lives in the guards every handler already calls — [`crate::guards::
//! resolve_ingest_project`] requires `Ingest`, `resolve_read_project` requires `Read` — plus the
//! explicit [`ensure_scope`] calls on the config-write handlers. [`ROUTE_SCOPES`] is the
//! *declaration* of that map, and the test at the bottom holds it to every route string in
//! `main.rs`, so a route added without a decision about who may call it fails the build.

use lighttrack_core::Scope;

use crate::auth::Principal;
use crate::error::ApiError;

/// The route table below is a **compile-time contract, not runtime dispatch**: enforcement happens
/// in the guards (which know the principal without a second store read), and a parallel dispatch
/// table would be a second source of truth that could drift from them silently. So it is compiled
/// only for the test that holds it against `build_router` — the check that a route cannot be added
/// without someone deciding who may call it.
#[cfg(test)]
mod table {
    use super::*;

    /// Who may call a route, for one HTTP method family.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Access {
        /// Admin (or dev-mode) principals only — no project key reaches it, whatever its scopes.
        Admin,
        /// A project key carrying this scope, or an admin.
        Key(Scope),
        /// No route method of that shape exists.
        None,
    }

    /// One route's declared access, split by method family: `read` covers `GET`, `write` covers
    /// `POST`/`PUT`/`DELETE`.
    pub(crate) struct RouteScope {
        pub(crate) path: &'static str,
        pub(crate) read: Access,
        pub(crate) write: Access,
    }

    const fn r(path: &'static str, read: Access, write: Access) -> RouteScope {
        RouteScope { path, read, write }
    }

    use Access::{Admin, Key, None as NoMethod};
    const INGEST: Access = Key(Scope::Ingest);
    const READ: Access = Key(Scope::Read);
    const MANAGE: Access = Key(Scope::Manage);

    /// Every `/v1/*` route and the capability a project key needs for it. Kept in the same order as
    /// `build_router` so the two read as one list.
    pub(crate) const ROUTE_SCOPES: &[RouteScope] = &[
        r("/v1/capabilities", READ, NoMethod),
        r("/v1/events", READ, INGEST),
        r("/v1/events/batch", NoMethod, INGEST),
        r("/v1/ingest/status", Admin, NoMethod),
        r("/v1/storage/status", Admin, NoMethod),
        r("/v1/events/:id", READ, NoMethod),
        // The OTLP door writes through the native batch handler, so it carries the batch's scope.
        r("/v1/traces", READ, INGEST),
        r("/v1/traces/:id", READ, NoMethod),
        // Scoring a trace reads it first and then writes an observability record — the same shape as
        // `POST /v1/scores`, and so the same capability, not `manage`.
        r("/v1/traces/:id/score", NoMethod, INGEST),
        r("/v1/costs", READ, NoMethod),
        r("/v1/costs/prompts", READ, NoMethod),
        // The unpriced-traffic ledger. Project-scoped like the cost rollups it qualifies — a
        // project key must be able to see that its OWN cost numbers are a floor.
        r("/v1/costs/unpriced", READ, NoMethod),
        r("/v1/usecases", READ, NoMethod),
        // The grouped primitive behind every cost surface. Project-scoped like the fixed
        // rollups above; the `api_key` dimension is gated to admins inside the handler.
        r("/v1/rollup", READ, NoMethod),
        r("/v1/scores", READ, INGEST),
        r("/v1/prices", READ, NoMethod),
        // The price timeline is a read of the same book `/v1/prices` exposes, so the same scope.
        r("/v1/prices/history/:provider/:model", READ, NoMethod),
        r("/v1/prices/:provider/:model", NoMethod, Admin),
        r("/v1/projects/:id/datasets", READ, Admin),
        r("/v1/datasets/:id", READ, NoMethod),
        r("/v1/datasets/:id/items", READ, Admin),
        r("/v1/datasets/:id/freeze", NoMethod, Admin),
        // M11 — the human verdict ledger. Writing a label is MANAGE, not INGEST: a label is a
        // configuration of what "good" means, and an ingest key that could move ground truth would
        // let the thing being measured edit the measurement.
        r("/v1/datasets/:id/items/from-label", NoMethod, Admin),
        r("/v1/labels", READ, MANAGE),
        r("/v1/calibrations", READ, MANAGE),
        r("/v1/judges/trust", READ, NoMethod),
        r("/v1/projects/:id/rubrics", READ, Admin),
        r("/v1/rubrics/:id", READ, NoMethod),
        // Minting a rubric generation changes what every future verdict means; admin, like every
        // other rubric write.
        r("/v1/rubrics/:id/versions", NoMethod, Admin),
        r("/v1/projects/:id/benchmarks", READ, Admin),
        r("/v1/benchmarks/:id", READ, NoMethod),
        r("/v1/benchmarks/:id/runs", READ, NoMethod),
        r("/v1/benchmarks/:id/gate", READ, NoMethod),
        // The one benchmark write a project key can reach: recording a run's result.
        r("/v1/benchmark-runs", NoMethod, MANAGE),
        r("/v1/benchmarks/:id/enqueue", NoMethod, Admin),
        r("/v1/projects/:id/prompts", READ, Admin),
        // The PUT links a prompt to its gating benchmark — a change to what gates a deploy, so admin.
        r("/v1/projects/:id/prompts/:name", READ, Admin),
        r("/v1/projects/:id/prompts/:name/versions", READ, Admin),
        r("/v1/projects/:id/prompts/:name/promote", NoMethod, Admin),
        r("/v1/jobs", Admin, Admin),
        r("/v1/jobs/claim", NoMethod, Admin),
        r("/v1/jobs/:id", Admin, NoMethod),
        r("/v1/jobs/:id/cancel", NoMethod, Admin),
        r("/v1/jobs/:id/progress", NoMethod, Admin),
        r("/v1/jobs/:id/renew", NoMethod, Admin),
        r("/v1/jobs/:id/finish", NoMethod, Admin),
        // Schedules are configuration of what this deployment spends money on, so writing one is
        // admin-only; a project key may READ its own project's schedules, like its own limits.
        r("/v1/projects/:id/schedules", READ, Admin),
        r("/v1/schedules", Admin, NoMethod),
        r("/v1/schedules/:id", NoMethod, Admin),
        r("/v1/schedules/:id/runs", Admin, NoMethod),
        r("/v1/projects", Admin, Admin),
        r("/v1/projects/:id", NoMethod, Admin),
        // The posture report names counts and a rule fingerprint, never payload text — so the
        // project's own read key may ask it. An operator who cannot check whether their own data
        // was scrubbed has a compliance answer they must take on faith.
        r("/v1/projects/:id/redaction", READ, NoMethod),
        r("/v1/projects/:id/keys", Admin, Admin),
        r("/v1/projects/:id/keys/:kid", NoMethod, Admin),
        r("/v1/projects/:id/keys/:kid/rotate", NoMethod, Admin),
        r("/v1/projects/:id/limits", READ, Admin),
        r("/v1/limits/:id", NoMethod, Admin),
        // A policy is a standing instruction to CREATE caps — strictly more power than creating one
        // cap — so a project key never reaches it, whatever its scopes.
        r("/v1/projects/:id/margin-policies", Admin, Admin),
        r("/v1/projects/:id/margin-policies/:pid", NoMethod, Admin),
        r("/v1/limits/status", READ, NoMethod),
        r("/v1/limits/usage", READ, NoMethod),
        r("/v1/relay/tasks", READ, INGEST),
        r("/v1/relay/tasks/:id", READ, NoMethod),
        // Device-key doors: `ensure_device` accepts the enrolled device key or an admin, never a
        // project key — so the settle report's `Ingest` character is enforced by the device gate.
        r("/v1/relay/tasks/:id/result", NoMethod, Admin),
        r("/v1/relay/lease", NoMethod, Admin),
        // Renew/progress are device-key doors, gated by `ensure_device` exactly like lease/result.
        r("/v1/relay/tasks/:id/renew", NoMethod, Admin),
        r("/v1/relay/tasks/:id/progress", NoMethod, Admin),
        // Cancel is the operator's, not the device's: the task's OWN project key reaches it (the
        // handler checks ownership), which is why it is not Admin-only like the device doors.
        r("/v1/relay/tasks/:id/cancel", NoMethod, MANAGE),
        // Device enrolment (M18) is admin-only in both directions, and `POST` mints a secret — so
        // it is also one of the routes that must never be reachable over MCP: a key in a tool
        // result is a key in a transcript. `GET` is admin rather than `READ` because the fleet is
        // operator infrastructure, not one project's data: a project key that could enumerate every
        // enrolled device would be reading across tenants.
        r("/v1/relay/devices", Admin, Admin),
        r("/v1/relay/devices/:id", NoMethod, Admin),
        r("/v1/revenue", NoMethod, Admin),
        // Admin-only, and never over MCP: this restates stored money in bulk.
        r("/v1/revenue/reprice", NoMethod, Admin),
        r("/v1/margin", Admin, NoMethod),
        r("/v1/margin/trend", Admin, NoMethod),
        r("/v1/margin/customer/:id", Admin, NoMethod),
        r("/v1/margin/simulate", Admin, NoMethod),
        r("/v1/forecast", READ, NoMethod),
        // The webhook door authenticates against the provider's signing secret, not a LightTrack key.
        r("/v1/billing/:provider/webhook", NoMethod, Admin),
        r("/v1/collective/digest", READ, NoMethod),
        r("/v1/collective/ingest", NoMethod, INGEST),
        r("/v1/collective/leaderboard", READ, NoMethod),
        r("/v1/collective/contribution", NoMethod, INGEST),
        // The ledger is an observability read like the events it is about, so a project key with
        // `read` sees its OWN project's alerts (`resolve_read_project` narrows it). Acknowledging
        // is a state change on shared operational record, so it needs `manage`.
        r("/v1/alerts", READ, NoMethod),
        r("/v1/alerts/:id/ack", NoMethod, MANAGE),
        // A resolution is written by the responder (an admin-keyed service), not by an app.
        r("/v1/alerts/:id/resolution", NoMethod, Admin),
        // Routing is where alerts GO: a project key that could add a channel could exfiltrate its
        // own alerts to any destination, so channel writes are admin, like every other config write.
        r("/v1/projects/:id/alert-channels", Admin, Admin),
        r("/v1/projects/:id/alert-channels/:cid", NoMethod, Admin),
        // Sending a real, signed test alert is a use of the deployment's own credentials.
        r("/v1/alert-channels/:id/test", NoMethod, Admin),
    ];
}

/// Does this principal carry `want`? Admin and dev principals pass everything — they are not keys in
/// the `api_keys` table, so there is no scope set to consult and no tenant to narrow them to.
pub(crate) fn ensure_scope(p: &Principal, want: Scope) -> Result<(), ApiError> {
    match p {
        Principal::Admin | Principal::Dev => Ok(()),
        Principal::Project { scopes, .. } if scopes.contains(&want) => Ok(()),
        Principal::Project { key_id, scopes, .. } => {
            // The denial names the key and both scope sets: the operator's fix is to mint a key with
            // the missing capability, and a bare "forbidden" told them nothing about which one.
            let have: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
            Err(ApiError::forbidden(format!(
                "key '{key_id}' is not scoped for this call: needs '{}', has [{}]. Mint a key with \
                 it: POST /v1/projects/:id/keys {{\"scopes\": [\"{}\"]}}",
                want.as_str(),
                have.join(", "),
                want.as_str(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::table::*;
    use super::*;

    /// Every `/v1/...` string literal in `build_router`.
    fn router_routes() -> Vec<String> {
        let src = include_str!("main.rs");
        let mut out = Vec::new();
        for (i, part) in src.split("\"/v1").enumerate() {
            if i == 0 {
                continue;
            }
            if let Some(end) = part.find('"') {
                out.push(format!("/v1{}", &part[..end]));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The point of the table: a route added without deciding who may call it fails here, rather
    /// than quietly inheriting whatever guard its handler happened to copy from a neighbour.
    #[test]
    fn every_v1_route_in_the_router_has_a_declared_scope() {
        let mut declared: Vec<&str> = ROUTE_SCOPES.iter().map(|r| r.path).collect();
        declared.sort();
        let routed = router_routes();
        assert!(
            !routed.is_empty(),
            "no routes found in main.rs — the extractor broke, not the router"
        );
        for path in &routed {
            assert!(
                declared.contains(&path.as_str()),
                "route {path} has no declared scope — add it to ROUTE_SCOPES"
            );
        }
        for path in &declared {
            assert!(
                routed.iter().any(|p| p == path),
                "ROUTE_SCOPES declares {path}, which build_router no longer serves"
            );
        }
    }

    /// A route that is neither readable nor writable by anyone is a typo, not a decision.
    #[test]
    fn no_route_is_declared_unreachable() {
        for r in ROUTE_SCOPES {
            assert!(
                r.read != Access::None || r.write != Access::None,
                "{} declares no method at all",
                r.path
            );
        }
    }

    fn key(scopes: &[Scope]) -> Principal {
        Principal::Project {
            project_id: "p".into(),
            key_id: "k".into(),
            scopes: scopes.to_vec(),
        }
    }

    #[test]
    fn a_narrow_key_is_refused_and_told_what_it_needs() {
        let ingest_only = key(&[Scope::Ingest]);
        assert!(ensure_scope(&ingest_only, Scope::Ingest).is_ok());
        let err = ensure_scope(&ingest_only, Scope::Read)
            .expect_err("an ingest-only key must not read")
            .to_string();
        assert!(err.starts_with("forbidden: "), "{err}");
        assert!(err.contains("needs 'read'"), "{err}");
        assert!(err.contains("has [ingest]"), "{err}");
    }

    /// Admin and dev are not keys, so scoping does not apply to them — the check must never
    /// accidentally lock an operator out of their own instance.
    #[test]
    fn admin_and_dev_pass_every_scope() {
        for p in [Principal::Admin, Principal::Dev] {
            for s in Scope::ALL {
                assert!(ensure_scope(&p, s).is_ok());
            }
        }
    }

    /// The back-compat default is what a key minted before scopes existed reads as; every route a
    /// project key could reach before this wave must still be reachable with it.
    #[test]
    fn the_back_compat_default_still_reaches_ingest_and_read() {
        let k = key(&lighttrack_core::default_scopes());
        assert!(ensure_scope(&k, Scope::Ingest).is_ok());
        assert!(ensure_scope(&k, Scope::Read).is_ok());
        assert!(
            ensure_scope(&k, Scope::Manage).is_err(),
            "the default is permissive, not unlimited — config writes still need `manage`"
        );
    }
}
