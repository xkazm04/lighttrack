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
//! of in a sixth parallel list.
//!
//! Since `access_layer`, the declaration *is* the enforcement: one router layer resolves every
//! request against that column before any extractor runs, and `tests_access_matrix` drives all 130
//! rows through the real `build_router` to prove each declared denial. The pin that used to live at
//! the bottom of this file — a text scan of `main.rs` compared against deduplicated path strings —
//! is gone, replaced by a guard that forbids its return.

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

    /// `src` with every `//` line comment removed. A source-scanning guard that matched its own
    /// prose would report a clean codebase in a voice indistinguishable from success.
    fn code_only(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The guard that replaced the bijection, and it pins **both** ends on purpose.
    ///
    /// What used to be here read `main.rs` as text, split it on `"/v1`, and compared the resulting
    /// path strings against `route_paths()`. It defaulted toward green in all three ways: its
    /// population was source text (so a comment mentioning a route satisfied it), its needle was a
    /// quoted path (so the method was invisible, `route_paths()` being deduplicated), and its
    /// haystack was one file. It also asserted nothing whatsoever about enforcement.
    ///
    /// `tests_access_matrix` does the real thing — it drives every row of the contract through
    /// `build_router` and asserts the declared denial — and `access_layer` is what makes that pass.
    /// So this guard asserts the new instruments exist and forbids a regression to text scanning.
    #[test]
    fn the_access_bijection_is_derived_from_the_router_not_from_source_text() {
        // Built in two pieces so the needle does not occur literally in this file and match itself.
        let text_pin = concat!("include_str!(\"main", ".rs\")");
        for (name, src) in [
            ("auth_scopes.rs", include_str!("auth_scopes.rs")),
            ("access_layer.rs", include_str!("access_layer.rs")),
            (
                "tests_access_matrix.rs",
                include_str!("tests_access_matrix.rs"),
            ),
            (
                "tests_access_probe.rs",
                include_str!("tests_access_probe.rs"),
            ),
        ] {
            assert!(
                !code_only(src).contains(text_pin),
                "{name} scans main.rs as text again — the access bijection is the probe in \
                 tests_access_matrix, driven through the real Router"
            );
        }

        // The layer resolves the axum route template, not a string it found in a file.
        let layer = code_only(include_str!("access_layer.rs"));
        assert!(
            layer.contains("MatchedPath"),
            "access_layer no longer resolves rows by MatchedPath"
        );
        assert!(
            layer.contains("e.path == template && e.method == method"),
            "access_layer no longer resolves rows by method AND path — a method mismatch would \
             become invisible again"
        );

        // And the probe still carries its floor: a derived selection that selects nothing must fail.
        let matrix = code_only(include_str!("tests_access_matrix.rs"));
        for needle in [
            "every_admin_row_refuses_a_project_key",
            "every_guarded_row_refuses_a_missing_credential",
            "every_declared_row_is_wired_with_its_declared_method",
            "the_matrix_covers_every_access_variant",
        ] {
            assert!(
                matrix.contains(needle),
                "tests_access_matrix lost {needle} — the access matrix is no longer derived"
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
