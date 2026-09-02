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
