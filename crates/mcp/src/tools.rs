//! Tool registry — combines the read + write catalogs and routes `tools/call`. Write tools are only
//! listed and callable when writes are enabled; otherwise calling one returns a clear, safe error.

use serde_json::{json, Value};

use crate::client::Client;
use crate::errors::map_error;
use crate::rpc::{more_results_line, tool_rendered, tool_text};
use crate::{prompts_tools, read, relay_tools, write};

/// The `tools/list` payload. Write tools appear only when `allow_writes`.
pub(crate) fn list(allow_writes: bool) -> Value {
    let mut tools = read::tools();
    tools.extend(prompts_tools::read_tools());
    // Relay reads are unconditional: they are side-effect-free, and the relay's WRITE surface
    // (enqueue, cancel, and above all device enrolment, which mints a key) is deliberately absent
    // from MCP entirely rather than gated behind the write flag.
    tools.extend(relay_tools::read_tools());
    if allow_writes {
        tools.extend(write::tools());
        tools.extend(prompts_tools::write_tools());
    }
    json!({ "tools": tools })
}

/// Handle `tools/call`, returning MCP tool-result content (text + isError).
///
/// **The door catches panics.** This transport is stdio: one process hosting one session. A handler
/// that panics — a slice index on a malformed API response, an `expect` in a renderer — takes the
/// whole session down with it, and the agent sees its connection die with no explanation, mid-task.
/// A panic in one tool call is a bug in that tool, not a reason to end the conversation, so it
/// becomes an in-band `isError` result like any other failure and the server keeps serving.
///
/// The two channels stay distinct: protocol-level faults (unknown method, bad params) are JSON-RPC
/// errors raised by the caller in `main`; anything that goes wrong *inside* a tool — including this
/// — is a tool result with `isError: true`, which is what an agent can actually read and act on.
pub(crate) fn call(c: &Client, allow_writes: bool, params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    guarded(&name, || call_inner(c, allow_writes, params))
}

/// Run one tool handler with the panic guard around it. Separate from [`call`] so the guard itself
/// is testable without an API to talk to — a guard nobody has watched catch anything is a comment.
fn guarded(name: &str, f: impl FnOnce() -> Value) -> Value {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            let detail = panic_message(payload.as_ref());
            // stderr, never stdout: stdout is the JSON-RPC channel (CONTRIBUTING.md's invariant),
            // and a panic message printed there would corrupt the protocol on top of the bug.
            eprintln!("lt-mcp: tool '{name}' PANICKED: {detail}");
            tool_text(
                &format!(
                    "internal error: the '{name}' tool panicked ({detail}). This is a bug in \
                     lt-mcp, not something your arguments can fix — the session is still alive, so \
                     other tools remain usable. Details are on the server's stderr."
                ),
                true,
            )
        }
    }
}

/// The panic's own message, when it is one of the two shapes `panic!` produces.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn call_inner(c: &Client, allow_writes: bool, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

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
    } else if let Some(r) = relay_tools::read_dispatch(c, name, &args) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The stdio transport hosts ONE session. Before this guard a panicking handler ended it, and
    /// the agent saw its connection die mid-task with no explanation. A panic is now an in-band tool
    /// error like any other failure, and the server survives to answer the next call.
    #[test]
    fn a_panicking_handler_becomes_a_tool_error_instead_of_ending_the_session() {
        let out = guarded("lt_events", || panic!("index out of bounds: the len is 0"));
        assert_eq!(out["isError"], true);
        let text = out["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("lt_events"), "{text}");
        assert!(text.contains("panicked"), "{text}");
        assert!(
            text.contains("index out of bounds"),
            "the panic's own message survives, or the report is unactionable: {text}"
        );
        assert!(
            text.contains("session is still alive"),
            "the agent must be told it can keep going: {text}"
        );

        // …and the guard is transparent when nothing goes wrong.
        let ok = guarded("lt_events", || tool_text("fine", false));
        assert_eq!(ok["isError"], false);
    }

    /// Both shapes `panic!` produces, plus the one it doesn't, so a panic is never reported as an
    /// empty reason.
    #[test]
    fn every_panic_payload_yields_a_reason() {
        assert_eq!(panic_message(&"literal"), "literal");
        assert_eq!(panic_message(&format!("formatted {}", 1)), "formatted 1");
        assert_eq!(panic_message(&42u8), "non-string panic payload");
    }
}
