//! `lt relay devices` — the operator's view of the device fleet (M18).
//!
//! Enrolment is a CLI job rather than an MCP one for a reason worth keeping visible: `add` prints a
//! device key, which is shown exactly once and stored only as a salted digest. A key that reached a
//! tool result would be a key in an agent transcript, so this door is HTTP and terminal only.

use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{Cli, RelayCmd, RelayDevicesCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &RelayCmd) -> Result<()> {
    match action {
        RelayCmd::Devices { action } => devices(cli, action),
    }
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
    use serde_json::Value;

    use super::*;

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
}
