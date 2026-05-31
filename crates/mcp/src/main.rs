//! `lt-mcp` — a Model Context Protocol server exposing LightTrack to Claude Code / agents.
//!
//! Transport: MCP stdio = newline-delimited JSON-RPC 2.0 on stdin/stdout. Every byte on stdout is
//! protocol; all diagnostics go to stderr. The server is a thin HTTP client of the LightTrack API
//! (so it works locally today and against Cloud Run later). It never touches the DB and cannot crash
//! the API process: read tools are side-effect-free; writes go through the validated API.
//!
//! Tools live in `read` (data gathering, always available) and `write` (enqueue runs + creates).
//! Writes are OFF by default — set `LIGHTTRACK_MCP_ALLOW_WRITES=1` to expose state-changing tools.
//!
//! Env: LIGHTTRACK_URL (default http://127.0.0.1:8787), LIGHTTRACK_KEY (admin or project key),
//!      LIGHTTRACK_MCP_ALLOW_WRITES (1/true/yes/on enables writes).

mod client;
mod read;
mod rpc;
mod tools;
mod write;

use std::io::{self, BufRead};

use serde_json::{json, Value};

use client::Client;

fn main() {
    let client = Client::from_env();
    let allow_writes = env_flag("LIGHTTRACK_MCP_ALLOW_WRITES");
    eprintln!(
        "lt-mcp v{} started (base={}, mode={})",
        env!("CARGO_PKG_VERSION"),
        client.base(),
        if allow_writes { "read+write" } else { "read-only" },
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("stdin read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("ignoring non-JSON line: {e}");
                continue;
            }
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        eprintln!("-> {method}");

        match method {
            "initialize" => rpc::send_result(&mut out, id, rpc::initialize_result(&params)),
            "tools/list" => rpc::send_result(&mut out, id, tools::list(allow_writes)),
            "tools/call" => {
                rpc::send_result(&mut out, id, tools::call(&client, allow_writes, &params))
            }
            "ping" => rpc::send_result(&mut out, id, json!({})),
            // Capability probes we don't implement — answer with empties to avoid client noise.
            "resources/list" => rpc::send_result(&mut out, id, json!({ "resources": [] })),
            "resources/templates/list" => {
                rpc::send_result(&mut out, id, json!({ "resourceTemplates": [] }))
            }
            "prompts/list" => rpc::send_result(&mut out, id, json!({ "prompts": [] })),
            // Notifications carry no id and need no response.
            _ if id.is_none() => {}
            other => rpc::send_error(&mut out, id, -32601, &format!("method not found: {other}")),
        }
    }
}

fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}
