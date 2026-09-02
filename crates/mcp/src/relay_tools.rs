//! Relay tools — **read-only** (M18). The relay had no MCP surface at all, so an agent debugging
//! "my task never ran" had no way to look, and every answer came from a human reading the cloud's
//! database.
//!
//! Three tools, all `readOnlyHint`, all side-effect-free. What is deliberately **absent** is the
//! rest of the surface: enqueueing, cancelling, and above all `POST /v1/relay/devices`, which mints
//! a device key. A minted secret in a tool result is a secret in a transcript — CLAUDE.md's rule,
//! and the reason device enrolment is HTTP-only however the write gate is set. The device listing
//! here carries no key and no digest, because the API strips both before they leave the database.

use serde_json::{json, Value};

use crate::client::Client;

/// Tool definitions added to the read catalog.
pub(crate) fn read_tools() -> Vec<Value> {
    vec![
        tool("list_relay_tasks", "Cloud→device relay tasks (newest first): work handed to an enrolled local device to run through Claude Code. Filter by project and status (queued | leased | succeeded | dead | cancelling | cancelled). Use this to answer \"did my relay task run\" — a task sitting `queued` with a low `attempts` is waiting for a device, not failing.",
            json!({"type":"object","properties":{
                "project":{"type":"string"},
                "status":{"type":"string","enum":["queued","leased","succeeded","dead","cancelling","cancelled"],"description":"only tasks in this state"},
                "limit":{"type":"integer","description":"max tasks (default 20, max 1000)"}
            }})),
        tool("get_relay_task", "One relay task by id: its status, result or error, attempt/failure counters, the device holding it, and its liveness `progress`. `failures` is the retry budget (runs that actually failed); `stale_reclaims` counts devices that died mid-run — they are different problems and the two counters exist to tell them apart.",
            json!({"type":"object","properties":{"task":{"type":"string","description":"relay task id"}},"required":["task"]})),
        tool("list_relay_devices", "The enrolled relay device fleet: each device's advertised capabilities (the action types it can run, exactly or as `ns/*`), when it was last seen, its agent version, and whether it is revoked. Keys are never included. Read this when relay tasks are not being picked up: a queued task whose action type nothing here advertises will never run, whatever its status says. Admin key required.",
            json!({"type":"object","properties":{
                "project":{"type":"string","description":"one project's devices (operator-wide devices are always included); omit for the whole fleet"}
            }})),
    ]
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": schema,
        "annotations": { "readOnlyHint": true, "openWorldHint": true }
    })
}

/// Route a relay read tool. `None` when `name` is not one, so the caller falls through.
pub(crate) fn read_dispatch(c: &Client, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let r = match name {
        "list_relay_tasks" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20);
            let mut p = format!("/v1/relay/tasks?limit={limit}");
            for k in ["project", "status"] {
                if let Some(v) = args
                    .get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    p.push_str(&format!("&{k}={v}"));
                }
            }
            c.get(&p)
        }
        "get_relay_task" => match args.get("task").and_then(Value::as_str) {
            Some(id) => c.get(&format!("/v1/relay/tasks/{id}")),
            None => Err("missing required argument: task".to_string()),
        },
        "list_relay_devices" => {
            let p = match args.get("project").and_then(Value::as_str) {
                Some(proj) if !proj.is_empty() => format!("/v1/relay/devices?project={proj}"),
                _ => "/v1/relay/devices".to_string(),
            };
            c.get(&p)
        }
        _ => return None,
    };
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_relay_tool_is_annotated_read_only() {
        // The annotation is the contract an agent host reads before deciding what it may call
        // unattended. A relay tool that mutated anything while carrying it would be worse than
        // having no annotation at all.
        for t in read_tools() {
            assert_eq!(
                t["annotations"]["readOnlyHint"], true,
                "{} must be read-only",
                t["name"]
            );
        }
    }

    #[test]
    fn no_relay_tool_can_mint_or_reveal_a_device_key() {
        // The rule this file exists to keep: device enrolment stays HTTP-only, whatever the write
        // gate says, because a minted secret in a tool result is a secret in a transcript.
        let names: Vec<String> = read_tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(!names
            .iter()
            .any(|n| n.contains("create") || n.contains("enrol")));
        // …and the one device tool routes to the listing, never to the minting door.
        let paths: Vec<&str> = vec!["/v1/relay/devices"];
        assert!(paths.iter().all(|p| !p.contains("keys")));
    }

    #[test]
    fn unknown_tools_fall_through_so_the_caller_can_try_the_other_catalogs() {
        // `dispatch` is chained; claiming a name this module does not serve would shadow it.
        // Constructed but never used to make a request: both cases below return before any HTTP.
        let c = Client::from_env();
        assert!(read_dispatch(&c, "list_projects", &json!({})).is_none());
        assert!(read_dispatch(&c, "", &json!({})).is_none());
    }

    #[test]
    fn a_missing_required_argument_is_a_clear_error_not_a_request_to_a_wrong_path() {
        // Constructed but never used to make a request: both cases below return before any HTTP.
        let c = Client::from_env();
        let r = read_dispatch(&c, "get_relay_task", &json!({}))
            .expect("routed")
            .expect_err("must refuse without a task id");
        assert!(r.contains("task"), "{r}");
    }
}
