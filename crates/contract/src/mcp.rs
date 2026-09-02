//! The MCP tool catalog, generated from the endpoint table.
//!
//! Tool names, argument names, argument types and the required set are a **contract**: agent
//! configurations address them by string, so a change here breaks somebody else's session at call
//! time. `crates/mcp/tool-contract.json` pins exactly that surface and the MCP crate's test holds
//! this generator to it.
//!
//! What is generated is the *catalog* — names, descriptions, input schemas, annotations. Dispatch
//! (which HTTP call a tool makes, how its arguments become a path or a body) stays in the MCP crate
//! beside the client, because that is I/O, not description.

use serde_json::{json, Map, Value};

use crate::types::{Endpoint, McpTool, Param};

/// The `tools/list` payload's `tools` array. Write tools appear only when `allow_writes`.
///
/// `output_schema` supplies the `outputSchema` for the tools that return `structuredContent`. It is
/// a callback rather than another column of this table because those schemas describe *response*
/// bodies — the MCP crate's hand-written, deliberately permissive views. What the contract owns is
/// the rule that a tool carries one exactly when its endpoint has a renderer, and the MCP crate
/// holds itself to that in a test. That rule is what the lagging `limit_rule` schema violated.
pub fn tools(allow_writes: bool, output_schema: impl Fn(&str) -> Option<Value>) -> Vec<Value> {
    crate::endpoints()
        .filter_map(|e| e.mcp.as_ref().map(|t| (e, t)))
        .filter(|(e, _)| allow_writes || !e.mutating)
        .map(|(e, t)| tool(e, t, &output_schema))
        .collect()
}

/// Is `name` a tool that changes state? Derived from the endpoint's `mutating` flag rather than a
/// second hand-kept name list — that list is what used to drift.
pub fn is_write_tool(name: &str) -> bool {
    crate::endpoints()
        .any(|e| e.mutating && e.mcp.as_ref().is_some_and(|t: &McpTool| t.name == name))
}

/// Every tool name in the catalog, writes included.
pub fn tool_names() -> Vec<&'static str> {
    crate::endpoints()
        .filter_map(|e| e.mcp.as_ref().map(|t| t.name))
        .collect()
}

/// The endpoint a tool name resolves to.
pub fn endpoint_for_tool(name: &str) -> Option<&'static Endpoint> {
    crate::endpoints().find(|e| e.mcp.as_ref().is_some_and(|t| t.name == name))
}

fn tool(e: &Endpoint, t: &McpTool, output_schema: &impl Fn(&str) -> Option<Value>) -> Value {
    let mut v = json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": input_schema(e, t),
        "annotations": annotations(e, t),
    });
    if let Some(out) = output_schema(t.name) {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("outputSchema".to_string(), out);
        }
    }
    v
}

/// The annotations an agent host reads before deciding what it may call unattended. `destructiveHint`
/// is false throughout: no tool in this catalog deletes stored observability data — `delete_limit`
/// removes a configuration rule, which is reversible by writing it again.
fn annotations(e: &Endpoint, t: &McpTool) -> Value {
    if t.read_only {
        json!({ "readOnlyHint": true, "openWorldHint": true })
    } else {
        json!({
            "readOnlyHint": false,
            "destructiveHint": false,
            "idempotentHint": t.idempotent || e.idempotent,
            "openWorldHint": true
        })
    }
}

/// The tool's `inputSchema`: its declared arguments, in declaration order, under the names an agent
/// passes them by.
fn input_schema(e: &Endpoint, t: &McpTool) -> Value {
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for name in t.args {
        let Some(p) = e.param(name) else { continue };
        props.insert(p.arg_name().to_string(), property(p));
        if p.required {
            required.push(json!(p.arg_name()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    Value::Object(schema)
}

/// One argument as a JSON Schema property. Shared with the OpenAPI renderer so a parameter cannot
/// describe itself one way to an agent and another way to a client generator.
pub(crate) fn property(p: &Param) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(p.ty.as_str()));
    if !p.enum_values.is_empty() {
        m.insert("enum".into(), json!(p.enum_values));
    }
    if !p.doc.is_empty() {
        m.insert("description".into(), json!(p.doc));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_tools_share_a_name() {
        let mut names = tool_names();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "a duplicate tool name shadows a tool");
    }

    /// Read-only mode must list a strict subset: gating writes may not change a read tool.
    #[test]
    fn disabling_writes_removes_exactly_the_mutating_tools() {
        let all: Vec<Value> = tools(true, |_| None);
        let reads: Vec<Value> = tools(false, |_| None);
        assert!(
            reads.len() < all.len(),
            "no write tools in the catalog at all?"
        );
        for t in &reads {
            assert_eq!(t["annotations"]["readOnlyHint"], true);
            assert!(
                all.contains(t),
                "a read tool changed shape when writes were off"
            );
        }
        for t in &all {
            let name = t["name"].as_str().unwrap_or_default();
            let listed = reads.iter().any(|r| r["name"] == t["name"]);
            assert_eq!(
                listed,
                !is_write_tool(name),
                "'{name}' is listed under read-only mode iff it is not a write tool"
            );
        }
    }

    /// A required argument must appear in `required`, or an agent will omit it and get a 400 it
    /// cannot diagnose from the schema it was given.
    #[test]
    fn required_arguments_are_declared_required() {
        for e in crate::endpoints() {
            let Some(t) = &e.mcp else { continue };
            let schema = input_schema(e, t);
            let req: Vec<&str> = schema["required"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for a in t.args {
                let p = e.param(a).expect("checked elsewhere");
                assert_eq!(
                    p.required,
                    req.contains(&p.arg_name()),
                    "{}: '{}' required-ness disagrees with the schema",
                    t.name,
                    p.arg_name()
                );
            }
        }
    }
}
