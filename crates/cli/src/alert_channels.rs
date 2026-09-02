//! `lt alerts channels` — where a project's alerts actually go.
//!
//! Two things here are deliberately CLI-only. `set --signed` mints a webhook signing secret that is
//! returned exactly once, and `test` spends the deployment's own credentials on a real delivery;
//! neither belongs in an agent transcript, which is why the contract declares no MCP tool for them.

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{AlertChannelsCmd, Cli};
use crate::http::call;

/// The transports the API's `kind` accepts. Checked here so a typo costs nothing rather than a 400
/// the operator has to decode.
const TRANSPORTS: &[&str] = &["webhook", "ntfy", "email"];
const SEVERITIES: &[&str] = &["info", "warning", "critical"];

pub(crate) fn run(cli: &Cli, action: &AlertChannelsCmd) -> Result<()> {
    match action {
        AlertChannelsCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/alert-channels"),
            None,
            "",
        ),
        AlertChannelsCmd::Set {
            project,
            kind,
            target,
            min_severity,
            kinds,
            disabled,
            signed,
        } => {
            let body = channel_body(
                kind,
                target,
                min_severity.as_deref(),
                kinds,
                *disabled,
                *signed,
            )?;
            call(
                cli,
                Method::PUT,
                &format!("/v1/projects/{project}/alert-channels"),
                Some(body),
                "",
            )
        }
        AlertChannelsCmd::Delete { project, id } => call(
            cli,
            Method::DELETE,
            &format!("/v1/projects/{project}/alert-channels/{id}"),
            None,
            "",
        ),
        AlertChannelsCmd::Test { id } => call(
            cli,
            Method::POST,
            &format!("/v1/alert-channels/{id}/test"),
            Some(json!({})),
            "",
        ),
    }
}

fn channel_body(
    kind: &str,
    target: &str,
    min_severity: Option<&str>,
    kinds: &[String],
    disabled: bool,
    signed: bool,
) -> Result<Value> {
    if !TRANSPORTS.contains(&kind) {
        bail!(
            "unknown channel type '{kind}': expected one of {}",
            TRANSPORTS.join(" | ")
        );
    }
    if let Some(sev) = min_severity {
        if !SEVERITIES.contains(&sev) {
            bail!(
                "unknown --min-severity '{sev}': expected one of {}",
                SEVERITIES.join(" | ")
            );
        }
    }
    let mut body = Map::new();
    body.insert("kind".into(), json!(kind));
    body.insert("target".into(), json!(target));
    body.insert("enabled".into(), json!(!disabled));
    body.insert("signed".into(), json!(signed));
    if let Some(sev) = min_severity {
        body.insert("min_severity".into(), json!(sev));
    }
    // An empty `--kind` list means "every kind", which the API spells as an absent field rather
    // than an empty array — sending `[]` would be a channel that wants nothing.
    if !kinds.is_empty() {
        body.insert("kinds".into(), json!(kinds));
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_transport_or_severity_is_refused_before_the_request() {
        assert!(channel_body("slack", "https://x", None, &[], false, false).is_err());
        assert!(channel_body("webhook", "https://x", Some("loud"), &[], false, false).is_err());
        assert!(channel_body("ntfy", "https://x", Some("critical"), &[], false, true).is_ok());
    }

    /// "Every kind" is the absence of the field. An empty array would be a channel that has opted
    /// out of everything, which is the opposite of what an operator who passed no `--kind` meant.
    #[test]
    fn no_kind_filter_sends_no_kinds_field() {
        let b = channel_body("webhook", "https://x", None, &[], false, false).expect("body");
        assert!(b.get("kinds").is_none(), "{b}");
        assert_eq!(b["enabled"], json!(true));
        assert_eq!(b["signed"], json!(false));

        let b = channel_body(
            "webhook",
            "https://x",
            None,
            &["score_drop".to_string()],
            true,
            true,
        )
        .expect("body");
        assert_eq!(b["kinds"], json!(["score_drop"]));
        assert_eq!(b["enabled"], json!(false));
        assert_eq!(b["signed"], json!(true));
    }
}
