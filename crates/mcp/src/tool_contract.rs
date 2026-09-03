//! The MCP tool surface as a **pinned contract**.
//!
//! Agent configurations name tools by string and pass arguments by name. A rename, a dropped tool,
//! a property that quietly changes type, or a newly-required argument breaks every config that was
//! working yesterday — silently, at call time, inside someone else's session. Nothing in the type
//! system holds that: the tool list is JSON assembled at runtime.
//!
//! So it is pinned here. `tool-contract.json` is a snapshot of `tools/list` (writes enabled, so the
//! gated half is covered too) reduced to what a caller can actually depend on: every tool name, its
//! `readOnlyHint`, the name and JSON type of every input property, and which of them are required.
//! Descriptions and prose are deliberately NOT pinned — rewording a description is a documentation
//! improvement, not a break.
//!
//! Regenerate deliberately, never to make a red test green:
//! `LIGHTTRACK_PIN_TOOL_CONTRACT=1 cargo test -p lighttrack-mcp tool_contract`.

#[cfg(test)]
use serde_json::{json, Map, Value};

/// The dependable shape of one tool: name, read-only hint, `property -> type`, required args.
#[cfg(test)]
fn shape(tool: &Value) -> (String, Value) {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let schema = tool
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut props = Map::new();
    if let Some(p) = schema.get("properties").and_then(Value::as_object) {
        for (k, v) in p {
            let ty = v
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("any")
                .to_string();
            props.insert(k.clone(), Value::String(ty));
        }
    }
    let mut required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    required.sort();
    let read_only = tool
        .pointer("/annotations/readOnlyHint")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (
        name,
        json!({ "readOnly": read_only, "properties": props, "required": required }),
    )
}

/// Every listed tool reduced to its pinned shape, keyed by name (so the file is stable under
/// reordering — where a tool appears in the list is not part of the contract).
#[cfg(test)]
fn surface(allow_writes: bool) -> Value {
    let listed = crate::tools::list(allow_writes);
    let mut out = Map::new();
    for t in listed["tools"].as_array().into_iter().flatten() {
        let (name, s) = shape(t);
        assert!(
            out.insert(name.clone(), s).is_none(),
            "two tools are both named '{name}' — the name is the whole addressing scheme"
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = include_str!("../tool-contract.json");

    #[test]
    fn the_tool_surface_matches_its_pinned_contract() {
        let current = surface(true);
        if std::env::var("LIGHTTRACK_PIN_TOOL_CONTRACT").is_ok() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tool-contract.json");
            let mut s = serde_json::to_string_pretty(&current).expect("json");
            s.push('\n');
            std::fs::write(path, &s).expect("write the pin");
            return;
        }
        let pinned: Value = serde_json::from_str(PINNED).expect("tool-contract.json parses");
        if pinned == current {
            return;
        }
        // Name the drift precisely: a whole-file diff of 50 tools is unreadable.
        let (p, c) = (
            pinned.as_object().expect("object"),
            current.as_object().expect("object"),
        );
        let mut problems = Vec::new();
        for (name, want) in p {
            match c.get(name) {
                None => problems.push(format!(
                    "tool '{name}' VANISHED — every agent config naming it now fails"
                )),
                Some(got) if got != want => problems.push(format!(
                    "tool '{name}' changed shape:\n  pinned:  {want}\n  current: {got}"
                )),
                Some(_) => {}
            }
        }
        for name in c.keys() {
            if !p.contains_key(name) {
                problems.push(format!(
                    "tool '{name}' is new — additions are fine, but re-pin deliberately"
                ));
            }
        }
        panic!(
            "the MCP tool surface drifted from its pinned contract:\n{}\n\nIf every change above \
             is intended, re-pin with LIGHTTRACK_PIN_TOOL_CONTRACT=1 cargo test -p lighttrack-mcp \
             tool_contract",
            problems.join("\n")
        );
    }

    /// The drift this item was named after: `limit_rule`'s `outputSchema` had fallen behind the
    /// input schema beside it, and nothing said so because the two lists were independent. They are
    /// no longer independent — a tool advertises structured output exactly when its endpoint has a
    /// renderer, because that is exactly when `tool_rendered` puts `structuredContent` in the result.
    #[test]
    fn a_tool_declares_an_output_schema_exactly_when_its_endpoint_has_a_renderer() {
        for t in crate::tools::list(true)["tools"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let name = t["name"].as_str().unwrap_or_default();
            let e = lighttrack_contract::mcp::endpoint_for_tool(name)
                .unwrap_or_else(|| panic!("'{name}' is listed but has no endpoint"));
            assert_eq!(
                t.get("outputSchema").is_some(),
                e.render_kind.is_some(),
                "'{name}': outputSchema and render_kind disagree. A tool renders Markdown and returns structuredContent together or not at all, so a caller reading the schema must not be promised a shape it will never receive."
            );
        }
    }

    /// Read-only mode must be a *subset*: gating writes may not change a read tool's shape.
    #[test]
    fn read_only_mode_lists_a_subset_with_identical_shapes() {
        let all = surface(true);
        let reads = surface(false);
        let all = all.as_object().expect("object");
        for (name, shape) in reads.as_object().expect("object") {
            assert_eq!(
                all.get(name),
                Some(shape),
                "'{name}' has a different shape when writes are disabled"
            );
            assert_eq!(
                shape["readOnly"], true,
                "'{name}' is listed without writes enabled but is not annotated read-only"
            );
        }
    }
}
