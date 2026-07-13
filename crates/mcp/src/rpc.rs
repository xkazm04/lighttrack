//! JSON-RPC 2.0 framing over stdio. stdout carries protocol bytes only; diagnostics go to stderr.

use std::io::Write;

use serde_json::{json, Value};

pub(crate) fn initialize_result(params: &Value) -> Value {
    // Echo the client's protocol version for maximum compatibility.
    let pv = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2024-11-05");
    json!({
        "protocolVersion": pv,
        "capabilities": { "tools": {}, "prompts": {}, "resources": {} },
        "serverInfo": { "name": "lighttrack-mcp", "version": env!("CARGO_PKG_VERSION") }
    })
}

/// Wrap text as an MCP tool-call result (`content` + `isError`).
pub(crate) fn tool_text(text: &str, is_error: bool) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error })
}

/// A rendered tool result: human-facing Markdown in `content` (what Claude Code shows + relays),
/// plus the exact raw object in `structuredContent` (arrays wrapped under `items`) for clients/agents
/// that consume structure. The Markdown carries full ids so follow-up calls work even where a client
/// drops `structuredContent`.
///
/// When `next_cursor` is set (a paged list with more rows remaining) it is surfaced both ways: in
/// `structuredContent.next_cursor` for structured clients, and as a trailing "More results available"
/// line on the Markdown so an agent reading only the text still knows to page.
pub(crate) fn tool_rendered(markdown: &str, raw: &Value, next_cursor: Option<&str>) -> Value {
    let mut structured = if raw.is_array() {
        json!({ "items": raw })
    } else {
        raw.clone()
    };
    let text = match next_cursor {
        Some(c) => {
            if let Some(obj) = structured.as_object_mut() {
                obj.insert("next_cursor".to_string(), Value::String(c.to_string()));
            }
            format!("{markdown}{}", more_results_line(c))
        }
        None => markdown.to_string(),
    };
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": structured,
        "isError": false
    })
}

/// The trailing pagination hint appended to a paged tool's Markdown output.
pub(crate) fn more_results_line(cursor: &str) -> String {
    format!("\n\n_More results available — call again with `cursor={cursor}` to fetch the next page._")
}

pub(crate) fn send_result(out: &mut impl Write, id: Option<Value>, result: Value) {
    send(out, json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result }));
}

pub(crate) fn send_error(out: &mut impl Write, id: Option<Value>, code: i64, message: &str) {
    send(
        out,
        json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": { "code": code, "message": message } }),
    );
}

fn send(out: &mut impl Write, msg: Value) {
    if writeln!(out, "{msg}").and_then(|_| out.flush()).is_err() {
        eprintln!("failed to write response");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_rendered_without_cursor_is_unchanged() {
        let raw = json!([{ "id": "e1" }]);
        let out = tool_rendered("# table", &raw, None);
        assert_eq!(out["content"][0]["text"], "# table");
        assert_eq!(out["structuredContent"]["items"], raw);
        assert!(out["structuredContent"].get("next_cursor").is_none());
    }

    #[test]
    fn tool_rendered_surfaces_cursor_both_ways() {
        let raw = json!([{ "id": "e1" }]);
        let out = tool_rendered("# table", &raw, Some("deadbeef"));
        assert_eq!(out["structuredContent"]["next_cursor"], "deadbeef");
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("# table"));
        assert!(text.contains("cursor=deadbeef"));
        assert!(text.contains("More results available"));
    }
}
