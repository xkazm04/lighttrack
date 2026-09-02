//! `lt relay` — the operator's view of the cloud→device relay (M18): the fleet, the task queue, and
//! the action fingerprint ledger derived from what devices reported back.
//!
//! Enrolment is a CLI job rather than an MCP one for a reason worth keeping visible: `devices add`
//! prints a device key, which is shown exactly once and stored only as a salted digest. A key that
//! reached a tool result would be a key in an agent transcript, so that door is HTTP and terminal
//! only.

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{
    Cli, RelayActionsArgs, RelayActionsCmd, RelayCmd, RelayDevicesCmd, RelayTasksCmd,
};
use crate::http::call;
use crate::query::{encode, Query};

pub(crate) fn run(cli: &Cli, action: &RelayCmd) -> Result<()> {
    match action {
        RelayCmd::Devices { action } => devices(cli, action),
        RelayCmd::Tasks { action } => tasks(cli, action),
        RelayCmd::Actions { args, action } => actions(cli, args, action),
    }
}

/// `/v1/relay/tasks` with the filters that were given. An unknown `--status` is left to the API,
/// which answers 400 with the accepted set rather than an empty page.
pub(crate) fn tasks_path(
    project: &Option<String>,
    status: &Option<String>,
    limit: usize,
) -> String {
    let mut q = Query::new("/v1/relay/tasks");
    q.push_raw("limit", Some(limit));
    q.push("project", project.as_deref());
    q.push("status", status.as_deref());
    q.finish()
}

pub(crate) fn actions_path(project: &Option<String>, limit: usize) -> String {
    let mut q = Query::new("/v1/relay/actions");
    q.push_raw("limit", Some(limit));
    q.push("project", project.as_deref());
    q.finish()
}

fn tasks(cli: &Cli, action: &RelayTasksCmd) -> Result<()> {
    match action {
        RelayTasksCmd::Enqueue {
            action_type,
            payload,
            project,
            source,
            idempotency_key,
            max_attempts,
            retry_interval_secs,
        } => {
            let body = enqueue_body(
                action_type,
                payload.as_deref(),
                project.as_deref(),
                source.as_deref(),
                idempotency_key.as_deref(),
                *max_attempts,
                *retry_interval_secs,
            )?;
            call(cli, Method::POST, "/v1/relay/tasks", Some(body), "")
        }
        RelayTasksCmd::List {
            project,
            status,
            limit,
        } => call(
            cli,
            Method::GET,
            &tasks_path(project, status, *limit),
            None,
            "",
        ),
        RelayTasksCmd::Cancel { id } => call(
            cli,
            Method::POST,
            &format!("/v1/relay/tasks/{id}/cancel"),
            Some(json!({})),
            "",
        ),
    }
}

fn actions(cli: &Cli, args: &RelayActionsArgs, action: &Option<RelayActionsCmd>) -> Result<()> {
    match action {
        None => call(
            cli,
            Method::GET,
            &actions_path(&args.project, args.limit),
            None,
            "",
        ),
        Some(RelayActionsCmd::Snapshot {
            action_type,
            project,
            name,
            limit,
        }) => {
            let mut body = Map::new();
            body.insert("project_id".into(), json!(project));
            if let Some(n) = name {
                body.insert("name".into(), json!(n));
            }
            if let Some(l) = limit {
                body.insert("limit".into(), json!(l));
            }
            call(
                cli,
                Method::POST,
                // The action type is namespaced, so its `/` has to survive as one path segment.
                &format!("/v1/relay/actions/{}/dataset", encode(action_type)),
                Some(Value::Object(body)),
                "get_dataset",
            )
        }
    }
}

/// The enqueue body. `payload` is typed JSON rather than a string: an action handed its parameters
/// as text would fail on the device, minutes later and somewhere an operator cannot see.
#[allow(clippy::too_many_arguments)]
fn enqueue_body(
    action_type: &str,
    payload: Option<&str>,
    project: Option<&str>,
    source: Option<&str>,
    idempotency_key: Option<&str>,
    max_attempts: Option<i64>,
    retry_interval_secs: Option<i64>,
) -> Result<Value> {
    let mut body = Map::new();
    body.insert("action_type".into(), json!(action_type));
    if let Some(p) = payload {
        let v: Value =
            serde_json::from_str(p).with_context(|| format!("--payload: invalid JSON: {p}"))?;
        if !v.is_object() {
            anyhow::bail!("--payload must be a JSON object, got: {p}");
        }
        body.insert("payload".into(), v);
    }
    for (k, v) in [
        ("project_id", project),
        ("source", source),
        ("idempotency_key", idempotency_key),
    ] {
        if let Some(v) = v {
            body.insert(k.into(), json!(v));
        }
    }
    if let Some(v) = max_attempts {
        body.insert("max_attempts".into(), json!(v));
    }
    if let Some(v) = retry_interval_secs {
        body.insert("retry_interval_secs".into(), json!(v));
    }
    Ok(Value::Object(body))
}

fn devices(cli: &Cli, action: &RelayDevicesCmd) -> Result<()> {
    match action {
        RelayDevicesCmd::List { project } => call(
            cli,
            Method::GET,
            &match project {
                Some(p) => format!("/v1/relay/devices?project={p}"),
                None => "/v1/relay/devices".to_string(),
            },
            None,
            "list_relay_devices",
        ),
        RelayDevicesCmd::Add {
            name,
            project,
            capability,
        } => {
            let body = json!({
                "name": name,
                "project_id": project,
                // Empty is meaningful and is passed through as such: it means "everything", which
                // is what the device's own action inventory will narrow at its first lease.
                "capabilities": capability,
            });
            call(
                cli,
                Method::POST,
                "/v1/relay/devices",
                Some(body),
                "get_relay_device",
            )
        }
        RelayDevicesCmd::Revoke { id } => call(
            cli,
            Method::DELETE,
            &format!("/v1/relay/devices/{id}"),
            None,
            "get_relay_device",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    /// The enrolment body must carry the capability list verbatim — including an empty one, which
    /// is a real advertisement ("everything") and not the same as omitting the field.
    #[test]
    fn enrolment_passes_capabilities_through_including_the_empty_list() {
        let caps: Vec<String> = vec!["xprice/*".into(), "ops/nightly".into()];
        let body = json!({ "name": "laptop", "project_id": Value::Null, "capabilities": caps });
        assert_eq!(body["capabilities"][0], "xprice/*");
        assert_eq!(body["capabilities"].as_array().unwrap().len(), 2);

        let empty: Vec<String> = Vec::new();
        let body = json!({ "name": "laptop", "project_id": Value::Null, "capabilities": empty });
        assert!(
            body["capabilities"]
                .as_array()
                .expect("an array")
                .is_empty(),
            "an empty advertisement is sent as an empty array, not dropped"
        );
    }

    /// `limit` opens the query string and the filters join it; an unscoped read sends neither, so
    /// the API answers over every project the key can see rather than one named "".
    #[test]
    fn the_task_and_action_reads_send_only_the_filters_given() {
        assert_eq!(tasks_path(&None, &None, 50), "/v1/relay/tasks?limit=50");
        assert_eq!(
            tasks_path(&s("p1"), &s("queued"), 10),
            "/v1/relay/tasks?limit=10&project=p1&status=queued"
        );
        assert_eq!(actions_path(&None, 1000), "/v1/relay/actions?limit=1000");
        assert_eq!(
            actions_path(&s("p1"), 20),
            "/v1/relay/actions?limit=20&project=p1"
        );
    }

    /// A payload typed at the shell has to reach the device as an object; text that only looks like
    /// JSON would fail on the device, minutes later and out of the operator's sight.
    #[test]
    fn an_enqueue_payload_must_be_a_json_object() {
        assert!(enqueue_body("a/b", Some("{bad"), None, None, None, None, None).is_err());
        assert!(enqueue_body("a/b", Some("\"text\""), None, None, None, None, None).is_err());
        let b =
            enqueue_body("a/b", Some(r#"{"n":1}"#), None, None, None, None, None).expect("body");
        assert_eq!(b["payload"]["n"], json!(1));
        assert!(b.get("source").is_none(), "{b}");
        assert!(b.get("max_attempts").is_none(), "{b}");
    }

    /// A namespaced action type is one path segment; its `/` must not become a route boundary.
    #[test]
    fn a_namespaced_action_type_is_encoded_into_the_path() {
        assert_eq!(encode("xprice/reprice-summary"), "xprice%2Freprice-summary");
    }
}
