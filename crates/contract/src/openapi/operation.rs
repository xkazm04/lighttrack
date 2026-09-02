//! One OpenAPI operation object: its parameters, request body, responses and the prose that
//! says who may call it. Split from `mod.rs` only for size — it is the same renderer.

use serde_json::{json, Map, Value};

use super::SchemaResolver;
use crate::mcp::property;
use crate::types::{Access, Endpoint, Param, ParamKind, TypeRef};

pub(super) fn operation(e: &Endpoint, resolve: SchemaResolver<'_>) -> Value {
    let mut op = Map::new();
    op.insert("operationId".into(), json!(e.id));
    op.insert("summary".into(), json!(e.doc));
    op.insert("description".into(), json!(describe(e)));
    let params: Vec<Value> = e
        .params
        .iter()
        .filter(|p| p.kind != ParamKind::Body)
        .map(parameter)
        .collect();
    if !params.is_empty() {
        op.insert("parameters".into(), Value::Array(params));
    }
    if let Some(body) = request_body(e, resolve) {
        op.insert("requestBody".into(), body);
    }
    op.insert("responses".into(), responses(e, resolve));
    if matches!(e.access, Access::Unauthenticated) {
        // An empty security array is OpenAPI's way of saying "this one takes no credential",
        // overriding the document-level default.
        op.insert("security".into(), json!([]));
    }
    Value::Object(op)
}

/// The prose an operation carries beyond its one-line summary: who may call it, and whether it is
/// paged. Both are facts a client generator and a human reader need and neither can infer.
fn describe(e: &Endpoint) -> String {
    let who = match e.access {
        Access::Admin => "Admin (or dev-mode) principals only.".to_string(),
        Access::Key(s) => format!(
            "An admin, or a project key carrying the `{}` capability.",
            s.as_str()
        ),
        Access::Unauthenticated => {
            "No LightTrack principal — authenticated by the caller's own signature, or not at all."
                .to_string()
        }
    };
    let mut s = format!("{}\n\n{who}", e.doc);
    if e.paged {
        s.push_str(
            "\n\nPaged: the next page's keyset cursor is returned in the `X-Next-Cursor` response \
             header; pass it back as `cursor`.",
        );
    }
    s
}

fn parameter(p: &Param) -> Value {
    json!({
        "name": p.name,
        "in": if p.kind == ParamKind::Path { "path" } else { "query" },
        "required": p.required,
        "description": p.doc,
        "schema": property(p),
    })
}

fn request_body(e: &Endpoint, resolve: SchemaResolver<'_>) -> Option<Value> {
    let fields: Vec<&Param> = e
        .params
        .iter()
        .filter(|p| p.kind == ParamKind::Body)
        .collect();
    let schema = match (e.body, fields.is_empty()) {
        (Some(t), _) => schema_for(t, resolve),
        (None, false) => {
            let mut props = Map::new();
            let mut required = Vec::new();
            for p in &fields {
                props.insert(p.name.to_string(), property(p));
                if p.required {
                    required.push(json!(p.name));
                }
            }
            let mut m = Map::new();
            m.insert("type".into(), json!("object"));
            m.insert("properties".into(), Value::Object(props));
            if !required.is_empty() {
                m.insert("required".into(), Value::Array(required));
            }
            Value::Object(m)
        }
        (None, true) => return None,
    };
    Some(json!({
        "required": true,
        "content": { "application/json": { "schema": schema } }
    }))
}

fn responses(e: &Endpoint, resolve: SchemaResolver<'_>) -> Value {
    let ok = match e.response {
        TypeRef::Empty => json!({ "description": "No content." }),
        t => json!({
            "description": "Success.",
            "content": { "application/json": { "schema": schema_for(t, resolve) } }
        }),
    };
    json!({
        "200": ok,
        "401": { "description": "Missing or invalid credential." },
        "403": { "description": "The principal lacks the capability this operation declares." },
        "501": {
            "description":
                "This deployment's store backend does not implement the surface. A permanent gap \
                 on this backend, never 'you have no data' — see GET /v1/capabilities."
        }
    })
}

fn schema_for(t: TypeRef, resolve: SchemaResolver<'_>) -> Value {
    match t {
        TypeRef::Named(n) => reference(n, resolve),
        TypeRef::ArrayOf(n) => json!({ "type": "array", "items": reference(n, resolve) }),
        TypeRef::Empty => json!({}),
        TypeRef::Untyped(doc) => json!({
            "type": "object",
            "additionalProperties": true,
            "description": doc,
        }),
    }
}

/// A `$ref` when the name resolves; otherwise a permissive object that still says which type it
/// stands for, so an unresolvable name degrades to a vague document rather than a wrong one.
fn reference(name: &str, resolve: SchemaResolver<'_>) -> Value {
    if resolve(name).is_some() {
        json!({ "$ref": format!("#/components/schemas/{name}") })
    } else {
        json!({
            "type": "object",
            "additionalProperties": true,
            "description": format!("`{name}` (no JSON Schema is registered for it in this build)"),
        })
    }
}
