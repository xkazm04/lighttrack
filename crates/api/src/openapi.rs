//! `GET /openapi.json` — this deployment's own machine-readable description.
//!
//! Rendered from `lighttrack-contract` rather than kept by hand: a checked-in OpenAPI file would be
//! a sixth description of the same routes and would drift exactly like the five that came before
//! it. Unauthenticated, like `/health`: it names paths, parameters and types, never a row of
//! anyone's data, and a client generator that had to hold a key to read it would be one more secret
//! in one more CI job.

use std::sync::OnceLock;

use axum::Json;
use serde_json::Value;

use crate::schema_registry::schema_for_name;

/// Built once per process. The table is `const`, so the document cannot change under a running
/// server, and rendering it per request would be pure waste on a route CI hits in a loop.
fn document() -> &'static Value {
    static DOC: OnceLock<Value> = OnceLock::new();
    DOC.get_or_init(|| {
        lighttrack_contract::openapi::document(env!("CARGO_PKG_VERSION"), &schema_for_name)
    })
}

pub(crate) async fn get_openapi() -> Json<&'static Value> {
    Json(document())
}

/// The last PUBLISHED description of this API, committed beside the SDK contract vectors.
///
/// The baseline is the artifact, not the source. A check that diffs the table against itself can
/// only ever be green: whatever the source says today is, by construction, what it says. What a
/// self-hosted caller built against is the document a release actually served, so that is the file
/// that has to be in the repository. Refresh it deliberately, at a release, with
/// `LIGHTTRACK_UPDATE_SURFACE_BASELINE=1 cargo test -p lighttrack-api openapi` — the same shape as
/// the `LIGHTTRACK_UPDATE_FIXTURES` convention the SDK contract already uses.
#[cfg(test)]
const BASELINE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../clients/contract/openapi.baseline.json"
);

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::redact::Redactor;
    use crate::tests_ingest::setup;

    /// A document that does not describe every route it serves is worse than none: a generated
    /// client silently lacks the call, and the gap looks like the feature not existing.
    #[test]
    fn the_document_describes_every_endpoint_in_the_contract() {
        let d = document();
        assert_eq!(d["openapi"], "3.1.0");
        let paths = d["paths"].as_object().expect("paths");
        for e in lighttrack_contract::endpoints() {
            let templated: String = e
                .path
                .split('/')
                .map(|s| match s.strip_prefix(':') {
                    Some(n) => format!("{{{n}}}"),
                    None => s.to_string(),
                })
                .collect::<Vec<_>>()
                .join("/");
            let item = paths
                .get(&templated)
                .unwrap_or_else(|| panic!("{templated} is missing from the document"));
            let op = item
                .get(e.method.as_str())
                .unwrap_or_else(|| panic!("{templated} has no {} operation", e.method.as_str()));
            assert_eq!(op["operationId"], e.id);
        }
    }

    /// The half a hand-written document always gets wrong: every `$ref` must land on a component
    /// that is actually present.
    #[test]
    fn every_reference_resolves_to_a_declared_component() {
        let d = document();
        let schemas = d["components"]["schemas"]
            .as_object()
            .expect("components.schemas");
        let mut refs = Vec::new();
        collect_refs(d, &mut refs);
        assert!(
            !refs.is_empty(),
            "no $ref at all — the resolver is not wired"
        );
        for r in refs {
            let name = r
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected $ref target {r}"));
            assert!(
                schemas.contains_key(name),
                "$ref points at '{name}', which is not in components.schemas"
            );
        }
    }

    /// schemars emits its own `$defs` inside each schema; those are internal to the component and
    /// must not be mistaken for document-level references.
    fn collect_refs(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, val) in m {
                    if k == "$ref" {
                        if let Some(s) = val.as_str() {
                            if s.starts_with("#/components/schemas/") {
                                out.push(s.to_string());
                            }
                        }
                    } else {
                        collect_refs(val, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
            _ => {}
        }
    }

    /// It must be served, and served without a credential — that is the whole point of publishing
    /// a description of the API.
    #[tokio::test]
    async fn the_route_is_served_and_needs_no_key() {
        let (state, _store) = setup(Redactor::off());
        let app = crate::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(body["info"]["title"], "LightTrack API");
        assert!(body["paths"]["/v1/events"]["get"].is_object());
        // The security scheme is declared once and overridden only where a door takes no key.
        assert_eq!(
            body["components"]["securitySchemes"]["bearerAuth"]["type"],
            "http"
        );
        assert_eq!(
            body["paths"]["/health"]["get"]["security"],
            serde_json::json!([])
        );
    }
}

/// The removal gate. See `docs/DEPRECATION.md`.
///
/// One rule, and only one: a name the previous published document carried may not vanish from this
/// one unless that document already marked it `deprecated`. Additions are free, prose may change,
/// and a marked field may be removed the moment its release arrives — the check has no opinion on
/// any of that. It has an opinion on exactly the act the policy exists to govern, which is the
/// silent disappearance of something somebody's deployment is still sending.
#[cfg(test)]
mod removal_guard {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Value;

    use super::{document, BASELINE};

    /// Every addressable name in one document, mapped to whether it carried a removal marker.
    ///
    /// Three kinds, because those are the three a caller can actually depend on: an operation
    /// (`POST /v1/x`), one of its parameters or body fields, and a property of a named response
    /// schema. Anything deeper is inside a `$ref`ed component and is reached through the third.
    fn surface(doc: &Value) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        let marked = |v: &Value| {
            v.get("deprecated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if let Some(paths) = doc["paths"].as_object() {
            for (path, item) in paths {
                for (method, op) in item.as_object().into_iter().flatten() {
                    let key = format!("{} {path}", method.to_uppercase());
                    let op_marked = marked(op);
                    out.insert(key.clone(), op_marked);
                    for p in op["parameters"].as_array().into_iter().flatten() {
                        if let Some(n) = p["name"].as_str() {
                            out.insert(format!("{key} ?{n}"), op_marked || marked(p));
                        }
                    }
                    let body =
                        &op["requestBody"]["content"]["application/json"]["schema"]["properties"];
                    for (n, f) in body.as_object().into_iter().flatten() {
                        out.insert(format!("{key} .{n}"), op_marked || marked(f));
                    }
                }
            }
        }
        for (name, schema) in doc["components"]["schemas"]
            .as_object()
            .into_iter()
            .flatten()
        {
            for (field, f) in schema["properties"].as_object().into_iter().flatten() {
                out.insert(format!("{name}.{field}"), marked(f));
            }
        }
        out
    }

    #[test]
    fn nothing_leaves_the_published_surface_unmarked() {
        let now = document();
        if std::env::var("LIGHTTRACK_UPDATE_SURFACE_BASELINE").is_ok() {
            std::fs::write(
                BASELINE,
                format!(
                    "{}
",
                    serde_json::to_string_pretty(now).expect("serialize")
                ),
            )
            .expect("write baseline");
            return;
        }
        let text = std::fs::read_to_string(BASELINE).unwrap_or_else(|e| {
            panic!("{BASELINE}: {e} — the published baseline is the gate's only honest input")
        });
        let previous: Value = serde_json::from_str(&text).expect("baseline is valid JSON");

        let before = surface(&previous);
        let after: BTreeSet<String> = surface(now).into_keys().collect();
        let unmarked: Vec<&str> = before
            .iter()
            .filter(|(k, marked)| !**marked && !after.contains(*k))
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(
            unmarked.is_empty(),
            "{} name(s) left the published surface without ever carrying a removal marker: {:#?}
             Mark them first (see docs/DEPRECATION.md), ship that release, then remove.",
            unmarked.len(),
            unmarked
        );
    }

    /// The measurable this direction buys, stated rather than asserted: how many marked elements
    /// the surface currently carries. Zero is a legitimate answer and means nothing is in flight;
    /// it is the instrument that must exist, not the number.
    #[test]
    fn the_surface_reports_how_much_of_it_is_going_away() {
        let n = surface(document()).values().filter(|m| **m).count();
        println!("surface elements carrying a removal marker: {n}");
    }
}
