//! `lt use-cases` — the registry of where this project calls an LLM, and the coverage report.

use anyhow::Result;
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{Cli, UseCasesCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &UseCasesCmd) -> Result<()> {
    match action {
        UseCasesCmd::Register {
            project,
            key,
            name,
            description,
            kind,
            status,
            component,
            expected_models,
            owner,
        } => {
            let mut body = Map::new();
            body.insert("key".into(), json!(key));
            body.insert("name".into(), json!(name));
            // Only send what was given: an omitted optional field must not overwrite a stored one
            // with null on a correction.
            for (k, v) in [
                ("description", description),
                ("kind", kind),
                ("status", status),
                ("component", component),
                ("owner", owner),
            ] {
                if let Some(v) = v {
                    body.insert(k.into(), json!(v));
                }
            }
            if !expected_models.is_empty() {
                body.insert("expected_models".into(), json!(expected_models));
            }
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/use-cases"),
                Some(Value::Object(body)),
                "upsert_use_case",
            )
        }
        UseCasesCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/use-cases"),
            None,
            "list_use_cases",
        ),
        UseCasesCmd::Show { project, key } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/use-cases/{key}"),
            None,
            "get_use_case",
        ),
        UseCasesCmd::Coverage { project, since } => {
            let q = since
                .as_deref()
                .map(|s| format!("?since={s}"))
                .unwrap_or_default();
            call(
                cli,
                Method::GET,
                &format!("/v1/projects/{project}/use-cases/coverage{q}"),
                None,
                "use_case_coverage",
            )
        }
        UseCasesCmd::Delete { project, key } => call(
            cli,
            Method::DELETE,
            &format!("/v1/projects/{project}/use-cases/{key}"),
            None,
            "delete_use_case",
        ),
    }
}
