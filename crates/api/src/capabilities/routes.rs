//! Which **routes** a backend refuses, derived from the endpoint table.
//!
//! `/v1/capabilities` published the refused *surfaces* and the renderer beside it printed "these
//! routes answer HTTP 501" over that list, so turning a refusal into the thing an operator can act
//! on — a method and a path — meant reading `docs/PARITY.md` (surface → `Store` method) and then
//! the handler. This module is that join, done once: `Capabilities::missing()` crossed with
//! `lighttrack_contract::endpoints()`.
//!
//! The join is in two parts on purpose, because collapsing them publishes a falsehood. A refused
//! surface either takes a route out in full (`Endpoint::surface`) or narrows one
//! (`Endpoint::degrades`) — and Postgres, the backend carrying production traffic, refuses
//! `maintenance` and `metrics`, **neither of which removes a single route**:
//! `GET /v1/storage/status` answers 200 with two null fields and an `unsupported` note. An
//! operator told to expect a 501 there goes looking for one that does not exist.

use lighttrack_store::Capabilities;
use serde::Serialize;

/// One route that still answers, with less in it than it would on a complete backend.
#[derive(Debug, Serialize)]
pub(crate) struct DegradedRoute {
    /// `"GET /v1/storage/status"` — the name an operator already knows the route by.
    pub route: String,
    /// The refused surfaces that narrow it: a filter that 501s, a field that comes back `null`, a
    /// list that comes back empty.
    pub surfaces: Vec<&'static str>,
}

/// `METHOD /path`, the one spelling of a route used across the manifest.
fn name(e: &lighttrack_contract::Endpoint) -> String {
    format!("{} {}", e.method.as_str().to_uppercase(), e.path)
}

/// The routes that answer HTTP 501 `unsupported` on this backend, in table order.
pub(crate) fn unsupported(caps: &Capabilities) -> Vec<String> {
    let missing = missing_names(caps);
    lighttrack_contract::endpoints()
        .filter(|e| missing.contains(&e.surface))
        .map(name)
        .collect()
}

/// The routes a refused surface narrows without removing.
///
/// A route already refused outright is left out: it cannot also be degraded, and listing it twice
/// would invite an operator to debug a partial answer they will never see.
pub(crate) fn degraded(caps: &Capabilities) -> Vec<DegradedRoute> {
    let missing = missing_names(caps);
    lighttrack_contract::endpoints()
        .filter(|e| !missing.contains(&e.surface))
        .filter_map(|e| {
            let surfaces: Vec<&'static str> = e
                .degrades
                .iter()
                .copied()
                .filter(|d| missing.contains(d))
                .collect();
            (!surfaces.is_empty()).then(|| DegradedRoute {
                route: name(e),
                surfaces,
            })
        })
        .collect()
}

fn missing_names(caps: &Capabilities) -> Vec<&'static str> {
    caps.missing().iter().map(|s| s.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_store::{SqliteStore, Surface};
    use lighttrack_store_firestore::FirestoreStore;
    use lighttrack_store_pg::PgStore;

    /// The two vocabularies are one vocabulary. The contract crate deliberately depends on nothing
    /// but `serde_json`, so `Surface` is not in scope there and the names are strings; this is the
    /// crate that sees both, and so this is where a typo or an unclaimed surface has to fail.
    #[test]
    fn every_surface_is_claimed_by_at_least_one_route() {
        let claimed = lighttrack_contract::claimed_surfaces();
        let unclaimed: Vec<&str> = Surface::ALL
            .iter()
            .map(|s| s.as_str())
            .filter(|n| !claimed.contains(n))
            .collect();
        assert!(
            unclaimed.is_empty(),
            "these surfaces can be refused by a backend and no endpoint row says what that costs, \
             so their refusal has no published consequence: {unclaimed:?}"
        );
    }

    /// The other direction: a surface name on a row that is not a real `Surface` would publish a
    /// refusal nothing can ever trigger, and a renamed surface would silently stop matching.
    #[test]
    fn every_claimed_surface_name_is_a_real_surface() {
        let real: Vec<&str> = Surface::ALL.iter().map(|s| s.as_str()).collect();
        for n in lighttrack_contract::claimed_surfaces() {
            assert!(
                real.contains(&n),
                "'{n}' is annotated on an endpoint row but is not a lighttrack_store::Surface"
            );
        }
        assert!(
            real.contains(&lighttrack_contract::CORE_SURFACE),
            "the default surface must itself be a real one"
        );
    }

    #[test]
    fn the_reference_backend_refuses_no_route() {
        let caps = SqliteStore::manifest();
        assert!(caps.missing().is_empty(), "SQLite is the reference backend");
        assert!(unsupported(&caps).is_empty());
        assert!(degraded(&caps).is_empty());
    }

    /// Postgres carries production traffic (Neon) and refuses three surfaces — and this is the
    /// finding the join produced: only ONE of the three removes any route. `maintenance` and
    /// `metrics` are published as refused today under a sentence promising a 501 that no route
    /// returns.
    #[test]
    fn postgres_refuses_the_use_case_routes_and_degrades_storage_status() {
        let caps = PgStore::manifest();
        let refused = unsupported(&caps);
        let mut expected: Vec<String> = lighttrack_contract::endpoints()
            .filter(|e| e.surface == "use_cases")
            .map(name)
            .collect();
        expected.sort();
        let mut got = refused.clone();
        got.sort();
        assert_eq!(got, expected, "refused routes on Postgres");
        assert!(
            refused
                .iter()
                .any(|r| r.contains("/v1/projects/:id/use-cases")),
            "the use-case registry 501s here: {refused:?}"
        );
        assert!(
            !refused.iter().any(|r| r.contains("/v1/storage/status")),
            "maintenance/metrics do NOT take this route out — it answers 200 with null fields"
        );

        let deg = degraded(&caps);
        let status = deg
            .iter()
            .find(|d| d.route == "GET /v1/storage/status")
            .expect(
                "a refused surface with no route-level consequence is published as degradation",
            );
        assert!(status.surfaces.contains(&"maintenance"));
        assert!(status.surfaces.contains(&"metrics"));
    }

    #[test]
    fn firestore_refuses_the_trace_and_redaction_routes() {
        let caps = FirestoreStore::manifest();
        let refused = unsupported(&caps);
        for want in [
            "GET /v1/traces",
            "GET /v1/traces/:id",
            "GET /v1/projects/:id/redaction",
        ] {
            assert!(
                refused.iter().any(|r| r == want),
                "{want} must be published as refused on firestore: {refused:?}"
            );
        }
        // Nothing a backend actually serves may be reported refused: that is the failure mode that
        // would send an operator looking for a gap that is not there.
        let served: Vec<&str> = caps.surfaces.iter().map(|s| s.as_str()).collect();
        for e in lighttrack_contract::endpoints() {
            if served.contains(&e.surface) {
                assert!(
                    !refused.contains(&name(e)),
                    "{} is on a served surface but was reported refused",
                    e.id
                );
            }
        }
        assert!(
            !refused.is_empty() && refused.len() < lighttrack_contract::endpoints().count(),
            "a derived selection that selects nothing — or everything — must not pass: {}",
            refused.len()
        );
    }
}
