//! Projects and their API keys (admin-only on the API side).

use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{Cli, KeysCmd, ProjectsCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &ProjectsCmd) -> Result<()> {
    match action {
        // `id` is sent only when the operator chose one: an explicit `null` would be a supplied id
        // as far as a stricter server is concerned, and the server's UUID is the right default.
        ProjectsCmd::Create { name, id } => {
            let mut body = json!({ "name": name });
            if let Some(id) = id {
                body["id"] = json!(id);
            }
            call(cli, Method::POST, "/v1/projects", Some(body), "")
        }
        ProjectsCmd::List => call(cli, Method::GET, "/v1/projects", None, "list_projects"),
    }
}

pub(crate) fn run_keys(cli: &Cli, action: &KeysCmd) -> Result<()> {
    match action {
        // `scopes` and `expires_at` are sent only when the operator named them: an explicit `null`
        // would read as "no scopes" to a stricter server, and the omitted-field default is the
        // documented back-compat one.
        KeysCmd::Create {
            project,
            name,
            scopes,
            expires,
        } => {
            let mut body = json!({ "name": name });
            if !scopes.is_empty() {
                body["scopes"] = json!(scopes);
            }
            if let Some(e) = expires {
                body["expires_at"] = json!(e);
            }
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/keys"),
                Some(body),
                "",
            )
        }
        KeysCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/keys"),
            None,
            "",
        ),
        KeysCmd::Rotate {
            project,
            id,
            grace_secs,
        } => {
            let mut body = json!({});
            if let Some(g) = grace_secs {
                body["grace_secs"] = json!(g);
            }
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/keys/{id}/rotate"),
                Some(body),
                "",
            )
        }
        KeysCmd::Revoke { project, id } => call(
            cli,
            Method::DELETE,
            &format!("/v1/projects/{project}/keys/{id}"),
            None,
            "",
        ),
    }
}
