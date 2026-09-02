//! `/openapi.json`, rendered from the endpoint table.
//!
//! The document is generated, never written: a hand-kept OpenAPI file is a sixth description of the
//! same routes and would drift like the five that came before it. Response schemas are the one part
//! this crate cannot produce alone — they live on types that derive `schemars::JsonSchema` in
//! `core`/`api` — so the caller passes a resolver, and a name it cannot resolve degrades to a
//! permissive object carrying the row's prose rather than silently vanishing.

mod operation;

use operation::operation;

use serde_json::{json, Map, Value};

use crate::types::TypeRef;

/// Resolve a `TypeRef` name to its JSON Schema. `None` means "this deployment has no schema for
/// that name" — the renderer then falls back to a described, permissive object.
pub type SchemaResolver<'a> = &'a dyn Fn(&str) -> Option<Value>;

/// The whole OpenAPI 3.1 document.
pub fn document(version: &str, resolve: SchemaResolver<'_>) -> Value {
    let mut paths: Map<String, Value> = Map::new();
    for e in crate::endpoints() {
        let entry = paths.entry(template(e.path)).or_insert_with(|| json!({}));
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(e.method.as_str().to_string(), operation(e, resolve));
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "LightTrack API",
            "version": version,
            "description":
                "Self-hosted LLM observability, LLM-as-judge scoring and benchmarking. This \
                 document is generated from `lighttrack-contract`, the single table the axum \
                 router, the MCP tool catalog, the `lt` CLI and the Markdown renderer are all held \
                 to by test.",
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description":
                        "An admin key, or a project key carrying the capability the operation \
                         declares (`ingest`, `read`, `manage`). Sent as `Authorization: Bearer …`.",
                }
            },
            "schemas": named_schemas(resolve),
        },
        "security": [ { "bearerAuth": [] } ],
        "paths": Value::Object(paths),
    })
}

/// Every `Named`/`ArrayOf` type the table mentions, resolved once into `components.schemas`.
fn named_schemas(resolve: SchemaResolver<'_>) -> Value {
    let mut out = Map::new();
    for name in named_types() {
        if let Some(s) = resolve(name) {
            out.insert(name.to_string(), s);
        }
    }
    Value::Object(out)
}

/// The distinct DTO names the table refers to. Public so the API can assert every one of them
/// resolves — a `TypeRef::Named` nobody can resolve is a typo, not a schema.
pub fn named_types() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = crate::endpoints()
        .flat_map(|e| {
            [Some(e.response), e.body]
                .into_iter()
                .flatten()
                .filter_map(|t| match t {
                    TypeRef::Named(n) | TypeRef::ArrayOf(n) => Some(n),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// axum's `:id` is OpenAPI's `{id}`.
fn template(path: &str) -> String {
    path.split('/')
        .map(|s| match s.strip_prefix(':') {
            Some(name) => format!("{{{name}}}"),
            None => s.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Access;

    fn doc() -> Value {
        document("test", &|_| None)
    }

    #[test]
    fn axum_path_parameters_become_openapi_templates() {
        assert_eq!(template("/v1/events/:id"), "/v1/events/{id}");
        assert_eq!(
            template("/v1/prices/history/:provider/:model"),
            "/v1/prices/history/{provider}/{model}"
        );
        assert_eq!(template("/v1/events"), "/v1/events");
    }

    /// The document must be structurally valid: every declared path present, every operation with a
    /// unique operationId, and every `{template}` backed by a `path` parameter.
    #[test]
    fn the_document_is_structurally_valid() {
        let d = doc();
        assert_eq!(d["openapi"], "3.1.0");
        let paths = d["paths"].as_object().expect("paths object");
        assert_eq!(
            paths.len(),
            crate::endpoints()
                .map(|e| template(e.path))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "one entry per distinct path"
        );
        let mut ids = Vec::new();
        for (path, item) in paths {
            for (method, op) in item.as_object().expect("path item") {
                assert!(
                    ["get", "post", "put", "delete"].contains(&method.as_str()),
                    "{path}: unexpected method {method}"
                );
                let id = op["operationId"].as_str().expect("operationId");
                ids.push(id.to_string());
                assert!(!op["summary"].as_str().unwrap_or_default().is_empty());
                assert!(
                    op["responses"]["200"].is_object(),
                    "{id}: no success response"
                );
                for seg in path.split('/').filter(|s| s.starts_with('{')) {
                    let name = seg.trim_matches(|c| c == '{' || c == '}');
                    let declared = op["parameters"]
                        .as_array()
                        .map(|a| a.iter().any(|p| p["name"] == name && p["in"] == "path"))
                        .unwrap_or(false);
                    assert!(declared, "{id}: template {seg} has no path parameter");
                }
            }
        }
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(before, ids.len(), "operationIds must be unique");
    }

    /// A resolver that knows a name must produce a `$ref` into `components.schemas`, and one that
    /// does not must still leave a usable, described object behind.
    #[test]
    fn named_types_reference_the_component_when_the_resolver_knows_them() {
        let known = |n: &str| (n == "LlmEvent").then(|| json!({"type": "object"}));
        let d = document("test", &known);
        assert!(d["components"]["schemas"]["LlmEvent"].is_object());
        let ev = &d["paths"]["/v1/events/{id}"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(ev["$ref"], "#/components/schemas/LlmEvent");

        let d = doc();
        let ev = &d["paths"]["/v1/events/{id}"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(ev["type"], "object");
        assert!(ev["description"]
            .as_str()
            .unwrap_or_default()
            .contains("LlmEvent"));
    }

    /// Unauthenticated doors must override the document-level requirement, or a generated client
    /// will refuse to call `/health` without a key it does not have.
    #[test]
    fn unauthenticated_operations_declare_empty_security() {
        let d = doc();
        for e in crate::endpoints() {
            let op = &d["paths"][template(e.path)][e.method.as_str()];
            match e.access {
                Access::Unauthenticated => assert_eq!(op["security"], json!([]), "{}", e.id),
                _ => assert!(op.get("security").is_none(), "{}", e.id),
            }
        }
    }
}
