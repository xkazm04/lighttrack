//! Tool registry — combines the read + write catalogs and routes `tools/call`. Write tools are only
//! listed and callable when writes are enabled; otherwise calling one returns a clear, safe error.

use serde_json::{json, Value};

use crate::client::Client;
use crate::rpc::tool_text;
use crate::{read, write};

/// The `tools/list` payload. Write tools appear only when `allow_writes`.
pub(crate) fn list(allow_writes: bool) -> Value {
    let mut tools = read::tools();
    if allow_writes {
        tools.extend(write::tools());
    }
    json!({ "tools": tools })
}

/// Handle `tools/call`, returning MCP tool-result content (text + isError).
pub(crate) fn call(c: &Client, allow_writes: bool, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let outcome = if let Some(r) = read::dispatch(c, name, &args) {
        r
    } else if write::is_write_tool(name) {
        if allow_writes {
            write::dispatch(c, name, &args)
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
        Ok(v) => tool_text(&serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(e) => tool_text(&format!("error: {e}"), true),
    }
}
