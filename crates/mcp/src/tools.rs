//! Tool registry — combines the read + write catalogs and routes `tools/call`. Write tools are only
//! listed and callable when writes are enabled; otherwise calling one returns a clear, safe error.

use serde_json::{json, Value};

use crate::client::Client;
use crate::errors::map_error;
use crate::rpc::{more_results_line, tool_rendered, tool_text};
use crate::{prompts_tools, read, write};

/// The `tools/list` payload. Write tools appear only when `allow_writes`.
pub(crate) fn list(allow_writes: bool) -> Value {
    let mut tools = read::tools();
    tools.extend(prompts_tools::read_tools());
    if allow_writes {
        tools.extend(write::tools());
        tools.extend(prompts_tools::write_tools());
    }
    json!({ "tools": tools })
}

/// Handle `tools/call`, returning MCP tool-result content (text + isError).
pub(crate) fn call(c: &Client, allow_writes: bool, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    // Paged read tools carry a keyset cursor out-of-band (the `X-Next-Cursor` header), so they route
    // through their own dispatch that returns `(body, next_cursor)`.
    if let Some(r) = read::dispatch_paged(c, name, &args) {
        return match r {
            Ok((v, cursor)) => render_result(name, &v, cursor.as_deref()),
            Err(e) => tool_text(&map_error(&e), true),
        };
    }

    let outcome = if let Some(r) = read::dispatch(c, name, &args) {
        r
    } else if let Some(r) = prompts_tools::read_dispatch(c, name, &args) {
        r
    } else if write::is_write_tool(name) || prompts_tools::is_write_tool(name) {
        if allow_writes {
            write::dispatch(c, name, &args)
                .or_else(|| prompts_tools::write_dispatch(c, name, &args))
                .unwrap_or_else(|| Err(format!("unknown tool: {name}")))
        } else {
            Err(format!(
                "tool '{name}' performs writes, which are disabled. Restart lt-mcp with LIGHTTRACK_MCP_ALLOW_WRITES=1 to enable."
            ))
        }
    } else {
        Err(format!("unknown tool: {name}"))
    };

    match outcome {
        Ok(v) => render_result(name, &v, None),
        Err(e) => tool_text(&map_error(&e), true),
    }
}

/// Shape a successful tool body into an MCP result: rendered Markdown + `structuredContent` when a
/// renderer matches, else pretty JSON. `next_cursor` (paged tools) is surfaced in both.
fn render_result(name: &str, body: &Value, next_cursor: Option<&str>) -> Value {
    match lighttrack_render::render(name, body) {
        Some(md) => tool_rendered(&md, body, next_cursor),
        None => {
            let mut text = serde_json::to_string_pretty(body).unwrap_or_default();
            if let Some(c) = next_cursor {
                text.push_str(&more_results_line(c));
            }
            tool_text(&text, false)
        }
    }
}
