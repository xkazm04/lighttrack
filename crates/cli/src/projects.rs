//! Projects and their API keys (admin-only on the API side).

use anyhow::Result;
use reqwest::Method;
use serde_json::json;

use crate::cli::{Cli, KeysCmd, ProjectsCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &ProjectsCmd) -> Result<()> {
    match action {
        ProjectsCmd::Create { name } => {
            call(cli, Method::POST, "/v1/projects", Some(json!({ "name": name })), "")
        }
        ProjectsCmd::List => call(cli, Method::GET, "/v1/projects", None, "list_projects"),
    }
}

pub(crate) fn run_keys(cli: &Cli, action: &KeysCmd) -> Result<()> {
    match action {
        KeysCmd::Create { project, name } => call(
            cli,
            Method::POST,
            &format!("/v1/projects/{project}/keys"),
            Some(json!({ "name": name })),
            "",
        ),
    }
}
