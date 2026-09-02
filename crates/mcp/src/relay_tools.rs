//! Relay tools — **read-only** (M18). The relay had no MCP surface at all, so an agent debugging
//! "my task never ran" had no way to look, and every answer came from a human reading the cloud's
//! database.
//!
//! Three tools, all `readOnlyHint`, all side-effect-free. What is deliberately **absent** is the
//! rest of the surface: enqueueing, cancelling, and above all `POST /v1/relay/devices`, which mints
//! a device key. A minted secret in a tool result is a secret in a transcript — CLAUDE.md's rule,
//! and the reason device enrolment is HTTP-only however the write gate is set. The device listing
//! here carries no key and no digest, because the API strips both before they leave the database.

use serde_json::Value;

use crate::client::Client;

/// Tool definitions added to the read catalog.
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
    use serde_json::json;

    use super::*;

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
