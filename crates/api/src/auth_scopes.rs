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
//! explicit [`ensure_scope`] calls on the config-write handlers. The *declaration* of that map used
//! to be a `ROUTE_SCOPES` table in this file; it is now one column of `lighttrack-contract`, so the
//! same fact is stated once beside the endpoint's parameters, response and MCP/CLI coverage instead
//! of in a sixth parallel list. The test at the bottom still holds it to every route string in
//! `main.rs`, so a route added without a decision about who may call it fails the build — it just
//! reads that decision from the contract now.

use lighttrack_core::Scope;

use crate::auth::Principal;
use crate::error::ApiError;

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
    /// than quietly inheriting whatever guard its handler happened to copy from a neighbour. The
    /// declaration moved to `lighttrack-contract`; the guarantee did not move with it, it stayed.
    #[test]
    fn every_v1_route_in_the_router_has_a_declared_scope() {
        let declared = lighttrack_contract::route_paths();
        let routed = router_routes();
        assert!(
            !routed.is_empty(),
            "no routes found in main.rs — the extractor broke, not the router"
        );
        for path in &routed {
            assert!(
                declared.contains(&path.as_str()),
                "route {path} has no declared scope — add an Endpoint row in crates/contract"
            );
        }
        for path in &declared {
            assert!(
                routed.iter().any(|p| p == path),
                "the contract declares {path}, which build_router no longer serves"
            );
        }
    }

    /// A route that is neither readable nor writable by anyone is a typo, not a decision.
    #[test]
    fn no_route_is_declared_unreachable() {
        for path in lighttrack_contract::route_paths() {
            let (read, write) = lighttrack_contract::access_for(path);
            assert!(
                read.is_some() || write.is_some(),
                "{path} declares no method at all"
            );
        }
    }

    /// The contract states scopes as strings so it can stay dependency-free. This is the seam where
    /// those strings become the enum the guards actually compare against — an unmappable one would
    /// mean the declaration and the enforcement had drifted apart in the one place they cannot.
    #[test]
    fn every_declared_key_scope_maps_onto_a_real_scope() {
        for e in lighttrack_contract::endpoints() {
            if let lighttrack_contract::Access::Key(k) = e.access {
                let s = Scope::ALL
                    .into_iter()
                    .find(|s| s.as_str() == k.as_str())
                    .unwrap_or_else(|| panic!("{}: '{}' is not a Scope", e.id, k.as_str()));
                assert!(ensure_scope(&key(&[s]), s).is_ok());
            }
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
